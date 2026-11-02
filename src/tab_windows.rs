//! Tabs in native windows of their own, each with its own tab bar. A tab
//! dropped outside the window it was dragged from (or right at its edge)
//! becomes one; dropped over empty space inside, it floats there as before.
//! Panes stay in the main window, where they follow its browser tabs.

use egui::{Pos2, Rect, Vec2, ViewportBuilder, ViewportId};
use egui_dock::{DockState, Surface, SurfaceIndex};
use serde::{Deserialize, Serialize};

use crate::dock_tab::DockTab;

/// A drop this close to a window's edge counts as outside it: the pointer
/// may stop being reported right as it leaves.
const EDGE: f32 = 4.0;

/// Size of a window a tab is dragged into.
pub const NEW_SIZE: Vec2 = Vec2::new(900.0, 620.0);

#[derive(Serialize, Deserialize)]
pub struct TabWindow {
    pub id: u64,
    pub dock: DockState<DockTab>,
    /// Where it was on the screen, in points, when the platform says.
    pub position: Option<Pos2>,
    pub size: Vec2,
    /// Opened for properties: properties opened later join it.
    #[serde(default)]
    pub properties: bool,
    /// Bring it to the front.
    #[serde(skip)]
    pub raise: bool,
    /// Shown already: where and how big it is is then up to the user.
    #[serde(skip)]
    opened: bool,
}

impl TabWindow {
    pub fn new(id: u64, dock: DockState<DockTab>, position: Option<Pos2>, size: Vec2) -> Self {
        Self {
            id,
            dock,
            position,
            size,
            properties: false,
            raise: true,
            opened: false,
        }
    }

    pub fn viewport_id(&self) -> ViewportId {
        ViewportId::from_hash_of(("tab-window", self.id))
    }

    /// The window as it opens; later only its title follows the tabs (egui
    /// applies what changes, so passing the size and position every frame
    /// would fight the user moving it).
    pub fn builder(&mut self) -> ViewportBuilder {
        let mut builder = ViewportBuilder::default()
            .with_title(self.title())
            .with_app_id("egui-files")
            .with_min_inner_size([320.0, 240.0]);
        if !std::mem::replace(&mut self.opened, true) {
            builder = builder.with_inner_size(self.size);
            if let Some(position) = self.position {
                builder = builder.with_position(position);
            }
        }
        builder
    }

    /// The shown tab's title, as the window's.
    pub fn title(&mut self) -> String {
        let shown = match self.dock.find_active_focused() {
            Some((_, tab)) => Some(tab.title()),
            None => self.dock.iter_all_tabs().next().map(|(_, t)| t.title()),
        };
        shown.map_or_else(|| "egui-files".to_string(), |t| plain_title(&t))
    }

    pub fn is_empty(&self) -> bool {
        self.dock.iter_all_tabs().next().is_none()
    }
}

/// A tab title without the icon it starts with: window titles can't show
/// the icon font (Phosphor's icons are in the private use area).
pub fn plain_title(title: &str) -> String {
    title
        .trim_start_matches(|c: char| ('\u{E000}'..='\u{F8FF}').contains(&c) || c.is_whitespace())
        .to_string()
}

/// Whether a drop at `pointer` (`None`: gone) is outside `screen`, the
/// window it was dragged in.
pub fn outside(screen: Rect, pointer: Option<Pos2>) -> bool {
    pointer.is_none_or(|p| !screen.shrink(EDGE).contains(p))
}

/// Takes the floating windows egui_dock made this frame (not yet shown)
/// out of `dock`, as docks of their own. Ones holding panes stay.
pub fn take_new_floating(dock: &mut DockState<DockTab>) -> Vec<DockState<DockTab>> {
    let new: Vec<SurfaceIndex> = dock
        .iter_surfaces_indexed()
        .filter_map(|(index, surface)| match surface {
            Surface::Window(tree, state)
                if !state.rect().is_positive()
                    && !tree
                        .iter()
                        .filter_map(|node| node.tabs())
                        .flatten()
                        .any(|tab| tab.pane().is_some()) =>
            {
                Some(index)
            }
            _ => None,
        })
        .collect();
    // Last first: removing the last surface shortens the list.
    new.into_iter()
        .rev()
        .filter_map(|index| match dock.remove_surface(index)? {
            Surface::Window(tree, _) => {
                let mut own = DockState::new(Vec::new());
                *own.main_surface_mut() = tree;
                Some(own)
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use egui_dock::{NodeIndex, TabIndex};

    use super::*;
    use crate::{dock_tab::PaneTab, tab::BrowserTab};

    fn browser(id: u64) -> DockTab {
        DockTab::Browser(BrowserTab::new(id, PathBuf::from("/")))
    }

    #[test]
    fn windows_come_back_after_a_restart() {
        let mut window = TabWindow::new(
            7,
            DockState::new(vec![browser(1), browser(2)]),
            Some(Pos2::new(40.0, 50.0)),
            Vec2::new(500.0, 400.0),
        );
        window.properties = true;
        // eframe saves app state as RON.
        let saved = ron::to_string(&window).unwrap();
        let mut back: TabWindow = ron::from_str(&saved).unwrap();
        assert_eq!(back.id, 7);
        assert!(back.properties);
        let ids: Vec<u64> = back.dock.iter_all_tabs().map(|(_, t)| t.id()).collect();
        assert_eq!(ids, [1, 2]);
        // It opens where it was, at its size, without taking the focus
        // from the main window.
        assert!(!back.raise);
        let builder = back.builder();
        assert_eq!(builder.position, Some(Pos2::new(40.0, 50.0)));
        assert_eq!(builder.inner_size, Some(Vec2::new(500.0, 400.0)));
        // After that, only the title.
        let builder = back.builder();
        assert_eq!((builder.position, builder.inner_size), (None, None));
    }

    #[test]
    fn titles_lose_their_icon() {
        assert_eq!(plain_title("\u{E25A} Documents"), "Documents");
        assert_eq!(plain_title("Plain"), "Plain");
    }

    #[test]
    fn drops_at_the_edge_count_as_outside() {
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        assert!(!outside(screen, Some(Pos2::new(400.0, 300.0))));
        assert!(outside(screen, Some(Pos2::new(-30.0, 300.0))));
        assert!(outside(screen, Some(Pos2::new(799.0, 300.0))));
        assert!(outside(screen, None));
    }

    #[test]
    fn new_floating_tabs_are_taken_but_panes_stay() {
        let mut dock = DockState::new(vec![browser(1), browser(2), browser(3)]);
        let leaf = egui_dock::NodePath {
            surface: SurfaceIndex::main(),
            node: NodeIndex::root(),
        };
        let at = |tab| egui_dock::TabPath {
            surface: leaf.surface,
            node: leaf.node,
            tab: TabIndex(tab),
        };
        dock.detach_tab(at(2), Rect::from_min_size(Pos2::ZERO, Vec2::splat(100.0)));
        dock.push_to_focused_leaf(DockTab::Pane(PaneTab {
            pane: crate::dock_tab::Pane::Places,
        }));
        let pane = dock
            .find_tab_from(|t| t.pane().is_some())
            .expect("pane docked");
        dock.detach_tab(pane, Rect::from_min_size(Pos2::ZERO, Vec2::splat(100.0)));

        let taken = take_new_floating(&mut dock);
        assert_eq!(taken.len(), 1);
        let ids: Vec<u64> = taken[0].iter_all_tabs().map(|(_, t)| t.id()).collect();
        assert_eq!(ids, [3]);
        // The pane still floats; tabs 1 and 2 are where they were.
        assert!(dock.find_tab_from(|t| t.pane().is_some()).is_some());
        assert_eq!(
            dock.main_surface()
                .iter()
                .filter_map(|n| n.tabs())
                .flatten()
                .count(),
            2
        );
    }
}
