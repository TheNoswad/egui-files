//! The kinds of tab the dock can hold.

use std::{collections::HashSet, path::PathBuf};

use egui::Ui;
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use crate::{
    archive::ArchiveTab,
    editor::EditorTab,
    properties::PropertiesTab,
    settings::SettingsTab,
    tab::{BrowserTab, Shared},
    trash::{self, TrashTab},
    usage::UsageTab,
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
    Pane(PaneTab),
    /// After the others: its saved form (`id`, `archive`) matches none of them.
    Archive(ArchiveTab),
    /// Saved as `id`, `edit`, which matches none of the others either.
    Editor(EditorTab),
    /// Saved as `id`, `usage`.
    Usage(UsageTab),
    /// Saved as `id`, `properties`.
    Properties(PropertiesTab),
    /// Saved as `id`, `settings`.
    Settings(SettingsTab),
}

/// A tool docked alongside the tabs. Each is open at most once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pane {
    Places,
    Tree,
    Preview,
    Terminal,
}

impl Pane {
    pub const ALL: [Self; 4] = [Self::Places, Self::Tree, Self::Preview, Self::Terminal];

    pub fn label(self) -> &'static str {
        match self {
            Self::Places => "Places",
            Self::Tree => "Folder tree",
            Self::Preview => "Preview",
            Self::Terminal => "Terminal",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Places => icon::COMPASS,
            Self::Tree => icon::TREE_STRUCTURE,
            Self::Preview => icon::EYE,
            Self::Terminal => icon::TERMINAL_WINDOW,
        }
    }

    /// Kept clear of browser and trash tab ids, which count up from 1.
    pub fn id(self) -> u64 {
        u64::MAX - self as u64
    }
}

#[derive(Serialize, Deserialize)]
pub struct PaneTab {
    pub pane: Pane,
}

impl DockTab {
    pub fn id(&self) -> u64 {
        match self {
            Self::Browser(tab) => tab.id,
            Self::Trash(tab) => tab.id,
            Self::Pane(tab) => tab.pane.id(),
            Self::Archive(tab) => tab.id,
            Self::Editor(tab) => tab.id,
            Self::Usage(tab) => tab.id,
            Self::Properties(tab) => tab.id,
            Self::Settings(tab) => tab.id,
        }
    }

    pub fn title(&self) -> String {
        match self {
            Self::Browser(tab) => tab.title(),
            Self::Trash(tab) => tab.title(),
            Self::Pane(tab) => format!("{} {}", tab.pane.icon(), tab.pane.label()),
            Self::Archive(tab) => tab.title(),
            Self::Editor(tab) => tab.title(),
            Self::Usage(tab) => tab.title(),
            Self::Properties(tab) => tab.title(),
            Self::Settings(tab) => tab.title(),
        }
    }

    pub fn browser(&self) -> Option<&BrowserTab> {
        match self {
            Self::Browser(tab) => Some(tab),
            Self::Trash(_)
            | Self::Pane(_)
            | Self::Archive(_)
            | Self::Editor(_)
            | Self::Usage(_)
            | Self::Properties(_)
            | Self::Settings(_) => None,
        }
    }

    pub fn browser_mut(&mut self) -> Option<&mut BrowserTab> {
        match self {
            Self::Browser(tab) => Some(tab),
            Self::Trash(_)
            | Self::Pane(_)
            | Self::Archive(_)
            | Self::Editor(_)
            | Self::Usage(_)
            | Self::Properties(_)
            | Self::Settings(_) => None,
        }
    }

    pub fn pane(&self) -> Option<Pane> {
        match self {
            Self::Pane(tab) => Some(tab.pane),
            Self::Browser(_)
            | Self::Trash(_)
            | Self::Archive(_)
            | Self::Editor(_)
            | Self::Usage(_)
            | Self::Properties(_)
            | Self::Settings(_) => None,
        }
    }

    pub fn is_trash(&self) -> bool {
        matches!(self, Self::Trash(_))
    }

    pub fn archive(&self) -> Option<&std::path::Path> {
        match self {
            Self::Archive(tab) => Some(&tab.archive),
            _ => None,
        }
    }

    pub fn properties(&self) -> Option<&PropertiesTab> {
        match self {
            Self::Properties(tab) => Some(tab),
            _ => None,
        }
    }

    pub fn usage(&self) -> Option<&UsageTab> {
        match self {
            Self::Usage(tab) => Some(tab),
            _ => None,
        }
    }

    pub fn editor(&self) -> Option<&EditorTab> {
        match self {
            Self::Editor(tab) => Some(tab),
            _ => None,
        }
    }

    pub fn editor_mut(&mut self) -> Option<&mut EditorTab> {
        match self {
            Self::Editor(tab) => Some(tab),
            _ => None,
        }
    }

    pub fn settings(&self) -> Option<&SettingsTab> {
        match self {
            Self::Settings(tab) => Some(tab),
            _ => None,
        }
    }

    /// Panes and settings need more of the app than tabs do, so the app
    /// draws them.
    pub fn ui(&mut self, ui: &mut Ui, shared: &mut Shared) {
        match self {
            Self::Browser(tab) => tab.ui(ui, shared),
            Self::Trash(tab) => tab.ui(ui, shared),
            Self::Archive(tab) => tab.ui(ui, shared),
            Self::Editor(tab) => tab.ui(ui, shared),
            Self::Usage(tab) => tab.ui(ui, shared),
            Self::Properties(tab) => tab.ui(ui, shared),
            Self::Pane(_) | Self::Settings(_) => {}
        }
    }

    pub fn mark_stale(&mut self) {
        match self {
            Self::Browser(tab) => tab.mark_stale(),
            Self::Trash(tab) => tab.mark_stale(),
            Self::Editor(tab) => tab.mark_stale(),
            Self::Usage(tab) => tab.mark_stale(),
            // Listings notice changes to the archive themselves; properties
            // look again every few seconds.
            Self::Pane(_) | Self::Archive(_) | Self::Properties(_) | Self::Settings(_) => {}
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
            Self::Editor(tab) => tab.dirs_changed(dirs),
            Self::Usage(tab) => tab.dirs_changed(dirs),
            Self::Pane(_) | Self::Archive(_) | Self::Properties(_) | Self::Settings(_) => {}
        }
    }

    /// Folders to watch for changes while this tab is open.
    pub fn shown_dirs(&self) -> Vec<PathBuf> {
        match self {
            Self::Browser(tab) => tab.shown_dirs().cloned().collect(),
            Self::Trash(_) => trash::watched_dir().into_iter().collect(),
            Self::Archive(tab) => tab.dir().into_iter().collect(),
            Self::Editor(tab) => tab.dir().into_iter().collect(),
            Self::Usage(tab) => tab.shown_dirs(),
            Self::Pane(_) | Self::Properties(_) | Self::Settings(_) => Vec::new(),
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
        new.push_to_first_leaf(DockTab::Pane(PaneTab {
            pane: Pane::Preview,
        }));
        new.push_to_first_leaf(DockTab::Archive(ArchiveTab::new(
            3,
            PathBuf::from("/tmp/a.zip"),
        )));
        new.push_to_first_leaf(DockTab::Editor(EditorTab::new(
            4,
            PathBuf::from("/tmp/notes.txt"),
        )));
        new.push_to_first_leaf(DockTab::Usage(UsageTab::new(5, PathBuf::from("/tmp"))));
        let mut settings = SettingsTab::new(6);
        settings.settings = crate::settings::Page::Places;
        new.push_to_first_leaf(DockTab::Settings(settings));
        let saved = ron::ser::to_string(&new).unwrap();
        let loaded: DockState<DockTab> = ron::from_str(&saved).unwrap();
        assert!(
            loaded
                .iter_all_tabs()
                .any(|(_, t)| t.usage().is_some_and(|u| u.id == 5))
        );
        assert!(loaded.iter_all_tabs().any(|(_, t)| {
            t.settings()
                .is_some_and(|s| s.id == 6 && s.settings == crate::settings::Page::Places)
        }));
        let kinds: Vec<_> = loaded
            .iter_all_tabs()
            .map(|(_, t)| {
                (
                    t.id(),
                    t.is_trash(),
                    t.pane(),
                    t.archive().is_some() || t.editor().is_some() || t.usage().is_some(),
                )
            })
            .collect();
        assert_eq!(
            kinds,
            [
                (1, false, None, false),
                (2, true, None, false),
                (Pane::Preview.id(), false, Some(Pane::Preview), false),
                (3, false, None, true),
                (4, false, None, true),
                (5, false, None, true),
                (6, false, None, false),
            ]
        );
    }
}
