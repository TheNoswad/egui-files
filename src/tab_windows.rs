//! Tabs in native windows of their own, each with its own tab bar. A tab
//! dropped outside the window it was dragged from (or right at its edge)
//! becomes one; dropped over empty space inside, it floats there as before.
//! Panes stay in the main window, where they follow its browser tabs.

use std::collections::HashMap;

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

/// Whether a tab may float over its window. Not the only tab of a tab
/// window: it would float over nothing; let go inside the window, it stays.
pub fn may_float(in_tab_window: bool, tabs: usize) -> bool {
    !in_tab_window || tabs > 1
}

/// Whether a drop at `pointer` (`None`: gone) is outside `screen`, the
/// window it was dragged in.
pub fn outside(screen: Rect, pointer: Option<Pos2>) -> bool {
    pointer.is_none_or(|p| !screen.shrink(EDGE).contains(p))
}

/// What a drop this frame took out of `dock` (drawn just now, in the
/// viewport `ctx` is running), with where on the screen each was dropped
/// when the platform says. Only on the frame the drag ends, and only if it
/// ended outside the window.
pub fn pop_out(
    ctx: &egui::Context,
    dock: &mut DockState<DockTab>,
    before: &Before,
) -> Vec<(DockState<DockTab>, Option<Pos2>)> {
    let (pointer, released, inner) = ctx.input(|i| {
        (
            i.pointer.hover_pos(),
            i.pointer.primary_released(),
            i.viewport().inner_rect,
        )
    });
    if !released || !outside(ctx.content_rect(), pointer) {
        return Vec::new();
    }
    let position = inner.zip(pointer).map(|(inner, p)| inner.min + p.to_vec2());
    take_dropped(dock, before)
        .into_iter()
        .map(|dock| (dock, position))
        .collect()
}

/// Tabs dropped outside the window they were dragged in, waiting a moment
/// to see whether another of the app's windows is under the pointer.
///
/// On Wayland windows don't know where they are on the screen, but once the
/// button is let go, the window under the pointer gets it (it enters). On
/// X11 and Windows the drop's screen position settles it right away.
pub struct Pending {
    pub docks: Vec<DockState<DockTab>>,
    /// The tab window they came from; `None`: the main one.
    pub source: Option<u64>,
    /// Where they were dropped, on the screen, when the platform says.
    pub screen: Option<Pos2>,
    /// When they were dropped (egui's clock).
    pub since: f64,
}

/// How long dropped tabs wait for a window to report the pointer before
/// opening in a window of their own.
pub const PENDING_WAIT: f64 = 0.4;

/// The main surface's leaf at `pos`, from where leaves were last drawn.
fn leaf_at(dock: &DockState<DockTab>, pos: Pos2) -> Option<egui_dock::NodePath> {
    dock.iter_leaves()
        .find(|(path, leaf)| path.surface.is_main() && leaf.rect.contains(pos))
        .map(|(path, _)| path)
}

/// Moves every tab of `docks` into `dock`: into the pane at `pos`, or the
/// focused one. Returns their ids.
pub fn insert_at(
    dock: &mut DockState<DockTab>,
    docks: Vec<DockState<DockTab>>,
    pos: Option<Pos2>,
) -> Vec<u64> {
    if let Some(leaf) = pos.and_then(|pos| leaf_at(dock, pos)) {
        dock.set_focused_node_and_surface(leaf);
    }
    let mut ids = Vec::new();
    for mut from in docks {
        for (_, node) in from.iter_all_nodes_mut() {
            if let Some(leaf) = node.get_leaf_mut() {
                for tab in leaf.tabs.drain(..) {
                    ids.push(tab.id());
                    dock.push_to_focused_leaf(tab);
                }
            }
        }
    }
    ids
}

/// Where every tab was before the dock was drawn, and which floating
/// windows were being moved, to tell what a drop changed.
pub struct Before {
    surfaces: HashMap<u64, SurfaceIndex>,
    moving: Vec<SurfaceIndex>,
    /// The tab being dragged by its title.
    dragged_tab: Option<u64>,
}

impl Before {
    /// `dock` before it's drawn, by the `DockArea` with the id `area`, in
    /// the viewport `ctx` is running.
    pub fn of(ctx: &egui::Context, dock: &DockState<DockTab>, area: egui::Id) -> Self {
        let moved = [ctx.dragged_id(), ctx.drag_stopped_id()];
        Self {
            surfaces: dock
                .iter_all_tabs()
                .map(|(path, tab)| (tab.id(), path.surface))
                .collect(),
            // egui_dock's id for dragging a tab by its title.
            dragged_tab: dock
                .iter_all_tabs()
                .find(|(path, _)| {
                    let id = area
                        .with((path.surface, "surface"))
                        .with((path.node, "node"))
                        .with((path.tab.0, "tab"));
                    moved.contains(&Some(id))
                })
                .map(|(_, tab)| tab.id()),
            moving: dock
                .iter_surfaces_indexed()
                .filter(|(_, s)| matches!(s, Surface::Window(..)))
                .map(|(index, _)| index)
                .filter(|&index| moved.contains(&Some(window_move_id(index))))
                .collect(),
        }
    }
}

/// What egui calls dragging a floating window of egui_dock's by its frame:
/// egui_dock names the window after its surface, and egui's move handle
/// goes by the window's id. (egui_dock's own `WindowState::dragged` is
/// never set.)
fn window_move_id(surface: SurfaceIndex) -> egui::Id {
    egui::Id::new(format!("window {surface:?}")).with("move")
}

/// Takes the floating windows a drop outside the window made or moved
/// (`before` being how the dock was before it was drawn this frame) out of
/// `dock`, as docks of their own. Ones holding panes stay floating.
///
/// Only call this on the frame the drag ended: during it, egui_dock keeps
/// where the dragged tab came from, and taking that window away would leave
/// it pointing at nothing.
pub fn take_dropped(dock: &mut DockState<DockTab>, before: &Before) -> Vec<DockState<DockTab>> {
    let dropped: Vec<SurfaceIndex> = dock
        .iter_surfaces_indexed()
        .filter_map(|(index, surface)| {
            let Surface::Window(tree, _) = surface else {
                return None;
            };
            let mut tabs = tree.iter().filter_map(|node| node.tabs()).flatten();
            let arrived = || {
                tree.iter()
                    .filter_map(|node| node.tabs())
                    .flatten()
                    .any(|tab| before.surfaces.get(&tab.id()) != Some(&index))
            };
            let has_pane = tabs.any(|tab| tab.pane().is_some());
            (!has_pane && (arrived() || before.moving.contains(&index))).then_some(index)
        })
        .collect();
    // Last first: removing the last surface shortens the list.
    let mut taken: Vec<DockState<DockTab>> = dropped
        .into_iter()
        .rev()
        .filter_map(|index| match dock.remove_surface(index)? {
            Surface::Window(tree, _) => {
                let mut own = DockState::new(Vec::new());
                *own.main_surface_mut() = tree;
                Some(own)
            }
            _ => None,
        })
        .collect();
    // A tab let go of outside every drop zone stays put in egui_dock.
    if let Some(id) = before.dragged_tab
        && let Some(path) = dock.find_tab_from(|t| t.id() == id && t.pane().is_none())
        && before.surfaces.get(&id) == Some(&path.surface)
        && let Some(tab) = dock.remove_tab(path)
    {
        taken.push(DockState::new(vec![tab]));
    }
    taken
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use egui_dock::NodeIndex;

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

    /// Drives egui_dock with real input: a dock showing `dock`, which each
    /// frame pops out what was dropped outside, as the app does.
    struct Drive {
        ctx: egui::Context,
        dock: DockState<DockTab>,
        popped: Vec<DockState<DockTab>>,
        time: f64,
        /// Tabs may float (egui_dock's floating windows).
        float: bool,
    }

    impl Drive {
        fn frame(&mut self, events: Vec<egui::Event>) {
            struct Blank(bool);
            impl egui_dock::TabViewer for Blank {
                type Tab = DockTab;
                fn title(&mut self, tab: &mut DockTab) -> egui::WidgetText {
                    format!("tab {}", tab.id()).into()
                }
                fn ui(&mut self, _ui: &mut egui::Ui, _tab: &mut DockTab) {}
                fn id(&mut self, tab: &mut DockTab) -> egui::Id {
                    egui::Id::new(tab.id())
                }
                fn allowed_in_windows(&self, _tab: &mut DockTab) -> bool {
                    self.0
                }
            }
            self.time += 0.1;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let (dock, popped, float) = (&mut self.dock, &mut self.popped, self.float);
            let mut output = self.ctx.run_ui(input, |ui| {
                let before = Before::of(ui.ctx(), dock, egui::Id::new("dock"));
                egui_dock::DockArea::new(dock)
                    .id(egui::Id::new("dock"))
                    .show_inside(ui, &mut Blank(float));
                popped.extend(pop_out(ui.ctx(), dock, &before).into_iter().map(|(d, _)| d));
            });
            output.textures_delta.clear();
        }

        fn button(&mut self, pos: Pos2, pressed: bool) {
            self.frame(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }]);
        }

        fn move_to(&mut self, pos: Pos2) {
            self.frame(vec![egui::Event::PointerMoved(pos)]);
        }
    }

    /// The crash this was written for: moving a floating window out of the
    /// main one, it was taken away mid-drag, and egui_dock indexed past the
    /// end of its surfaces. Now it pops out once let go.
    #[test]
    fn moving_a_floating_window_out_of_the_window() {
        let mut dock = DockState::new(vec![browser(1), browser(2)]);
        dock.detach_tab(
            main_tab(&dock, 2),
            Rect::from_min_size(Pos2::new(300.0, 200.0), Vec2::new(300.0, 200.0)),
        );
        let mut drive = Drive {
            ctx: egui::Context::default(),
            dock,
            popped: Vec::new(),
            time: 0.0,
            float: true,
        };
        drive.frame(Vec::new());
        drive.frame(Vec::new());
        // Where it was detached to (egui_dock doesn't report where its
        // floating windows are).
        let window = Rect::from_min_size(Pos2::new(300.0, 200.0), Vec2::new(300.0, 200.0));
        // By its frame, then out through the left edge and beyond.
        let grab = window.min + Vec2::new(30.0, 12.0);
        drive.move_to(grab);
        drive.button(grab, true);
        for x in [grab.x - 20.0, 200.0, 100.0, 10.0, -50.0, -120.0] {
            drive.move_to(Pos2::new(x, grab.y));
            assert!(drive.popped.is_empty(), "nothing pops out mid-drag");
        }
        drive.button(Pos2::new(-120.0, grab.y), false);
        drive.frame(Vec::new());
        assert_eq!(drive.popped.len(), 1);
        assert_eq!(ids(&drive.popped[0]), [2]);
        assert_eq!(ids(&drive.dock), [1]);
    }

    #[test]
    fn dragging_a_tab_out_of_the_window() {
        // A tab that may not float (a tab window's only one) still pops out.
        for float in [true, false] {
            drag_a_tab_out(float);
        }
    }

    fn drag_a_tab_out(float: bool) {
        let mut drive = Drive {
            ctx: egui::Context::default(),
            dock: DockState::new(vec![browser(1), browser(2), browser(3)]),
            popped: Vec::new(),
            time: 0.0,
            float,
        };
        drive.frame(Vec::new());
        // The first tab's title in the tab bar.
        let leaf = drive.dock.leaf(egui_dock::NodePath {
            surface: SurfaceIndex::main(),
            node: NodeIndex::root(),
        });
        let bar = leaf.unwrap().rect;
        let grab = Pos2::new(bar.min.x + 80.0, bar.min.y + 10.0);
        drive.move_to(grab);
        drive.button(grab, true);
        for y in [grab.y + 20.0, 300.0, 590.0, 640.0, 700.0] {
            drive.move_to(Pos2::new(grab.x, y));
            assert!(drive.popped.is_empty(), "nothing pops out mid-drag");
        }
        drive.button(Pos2::new(grab.x, 700.0), false);
        drive.frame(Vec::new());
        assert_eq!(drive.popped.len(), 1, "{:?}", ids(&drive.dock));
        assert_eq!(ids(&drive.popped[0]), [1]);
        assert_eq!(ids(&drive.dock), [2, 3]);
    }

    #[test]
    fn tabs_dropped_in_join_the_pane_under_the_pointer() {
        // Two panes side by side.
        let mut dock = DockState::new(vec![browser(1)]);
        dock.main_surface_mut()
            .split_right(NodeIndex::root(), 0.5, vec![browser(2)]);
        let mut drive = Drive {
            ctx: egui::Context::default(),
            dock,
            popped: Vec::new(),
            time: 0.0,
            float: true,
        };
        drive.frame(Vec::new());
        let dropped = vec![
            DockState::new(vec![browser(3), browser(4)]),
            DockState::new(vec![browser(5)]),
        ];
        let ids = insert_at(&mut drive.dock, dropped, Some(Pos2::new(700.0, 300.0)));
        assert_eq!(ids, [3, 4, 5]);
        let right = drive
            .dock
            .iter_leaves()
            .find(|(_, leaf)| leaf.tabs.iter().any(|t| t.id() == 2))
            .map(|(_, leaf)| leaf.tabs.iter().map(DockTab::id).collect::<Vec<_>>());
        assert_eq!(right, Some(vec![2, 3, 4, 5]));
    }

    #[test]
    fn a_tab_windows_only_tab_doesnt_float() {
        assert!(!may_float(true, 1));
        assert!(may_float(true, 2));
        // The main window's panes are tabs too, so it always has others.
        assert!(may_float(false, 1));
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

    fn main_tab(dock: &DockState<DockTab>, id: u64) -> egui_dock::TabPath {
        dock.find_tab_from(|t| t.id() == id).expect("open")
    }

    fn some_rect() -> Rect {
        Rect::from_min_size(Pos2::ZERO, Vec2::splat(100.0))
    }

    fn ids(dock: &DockState<DockTab>) -> Vec<u64> {
        dock.iter_all_tabs().map(|(_, t)| t.id()).collect()
    }

    #[test]
    fn tabs_dropped_out_are_taken_but_panes_stay() {
        let mut dock = DockState::new(vec![browser(1), browser(2), browser(3)]);
        dock.push_to_focused_leaf(DockTab::Pane(PaneTab {
            pane: crate::dock_tab::Pane::Places,
        }));
        let before = Before::of(&egui::Context::default(), &dock, egui::Id::new("dock"));
        dock.detach_tab(main_tab(&dock, 3), some_rect());
        let pane = dock.find_tab_from(|t| t.pane().is_some()).unwrap();
        dock.detach_tab(pane, some_rect());

        let taken = take_dropped(&mut dock, &before);
        assert_eq!(taken.len(), 1);
        assert_eq!(ids(&taken[0]), [3]);
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

    /// The crash this guards against: a floating window that a drop didn't
    /// change (say, the one a drag started from) mustn't be taken away.
    #[test]
    fn floating_windows_the_drop_left_alone_stay() {
        let mut dock = DockState::new(vec![browser(1), browser(2), browser(3)]);
        dock.detach_tab(main_tab(&dock, 3), some_rect());
        let before = Before::of(&egui::Context::default(), &dock, egui::Id::new("dock"));
        assert!(take_dropped(&mut dock, &before).is_empty());
        assert_eq!(dock.iter_all_tabs().count(), 3);
    }

    /// Dragging the only tab of a floating window out moves the window
    /// (egui_dock makes it anew), which counts as dropped.
    #[test]
    fn a_floating_window_dragged_out_is_taken() {
        let mut dock = DockState::new(vec![browser(1), browser(2)]);
        dock.detach_tab(main_tab(&dock, 2), some_rect());
        let before = Before::of(&egui::Context::default(), &dock, egui::Id::new("dock"));
        dock.detach_tab(main_tab(&dock, 2), some_rect());
        let taken = take_dropped(&mut dock, &before);
        assert_eq!(taken.len(), 1);
        assert_eq!(ids(&taken[0]), [2]);
        assert_eq!(ids(&dock), [1]);
    }
}
