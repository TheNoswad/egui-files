//! The terminal panel: terminals in tabs and splits, in a dock of their own.

use std::path::{Path, PathBuf};

use egui::{Context, Id, Key, Modifiers, Ui, WidgetText};
use egui_dock::{DockArea, DockState, Node, NodePath, Split, Style, TabViewer};
use egui_phosphor::regular as icon;

use serde::{Deserialize, Serialize};

use crate::terminal::{self, Follow, FollowState, Terminal, TerminalResponse};

/// What the app should do after the panel was drawn.
#[derive(Default)]
pub struct PanelResponse {
    /// A terminal's shell changed folder; show it in the focused tab.
    pub navigate: Option<PathBuf>,
    /// The last terminal closed.
    pub close: bool,
}

/// The layout is saved; the shells start afresh after a restart.
#[derive(Serialize, Deserialize)]
pub struct TerminalPanel {
    dock: DockState<Terminal>,
    next_id: u64,
    /// Terminals opened from now on follow the focused folder.
    #[serde(default)]
    new_follow: bool,
}

impl Default for TerminalPanel {
    fn default() -> Self {
        // The first terminal follows the focused folder; ones opened later
        // stay where they are unless asked to.
        Self {
            dock: DockState::new(vec![Terminal::new(1, true)]),
            next_id: 2,
            new_follow: false,
        }
    }
}

enum Action {
    /// Open a terminal in a new tab of this leaf, or split it off a terminal.
    Open {
        from: Option<u64>,
        split: Option<Split>,
        leaf: Option<NodePath>,
    },
    Close(u64),
    /// `id` is the terminal the change came from.
    Follow {
        id: u64,
        change: Follow,
    },
}

impl TerminalPanel {
    pub fn has_focus(&self, ctx: &Context) -> bool {
        let focused = ctx.memory(|m| m.focused());
        focused.is_some_and(|id| self.dock.iter_all_tabs().any(|(_, t)| t.widget_id() == id))
    }

    /// Focuses the active terminal when it's next drawn.
    pub fn focus(&mut self) {
        if let Some((_, terminal)) = self.dock.find_active_focused() {
            terminal.request_focus();
        } else if let Some((_, terminal)) = self.dock.iter_all_tabs_mut().next() {
            terminal.request_focus();
        }
    }

    pub fn surrender_focus(&self, ctx: &Context) {
        if self.has_focus(ctx) {
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
        }
    }

    /// Records where each shell is, ahead of saving the layout.
    pub fn remember_dirs(&mut self) {
        for (_, terminal) in self.dock.iter_all_tabs_mut() {
            terminal.remember_dir();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.dock.iter_all_tabs().next().is_none()
    }

    /// Ends every shell; the next `ui` call starts a single new one.
    pub fn reset(&mut self) {
        *self = Self {
            new_follow: self.new_follow,
            ..Self::default()
        };
    }

    fn follow_state(&self) -> FollowState {
        FollowState {
            all: self.dock.iter_all_tabs().all(|(_, t)| t.follow),
            new: self.new_follow,
        }
    }

    fn set_follow(&mut self, id: u64, change: Follow) {
        let set = |terminal: &mut Terminal, follow| {
            if terminal.follow != follow {
                terminal.set_follow(follow);
            }
        };
        match change {
            Follow::This(follow) => {
                if let Some(terminal) = self
                    .dock
                    .iter_all_tabs_mut()
                    .map(|(_, t)| t)
                    .find(|t| t.id == id)
                {
                    set(terminal, follow);
                }
            }
            Follow::All(follow) => {
                for (_, terminal) in self.dock.iter_all_tabs_mut() {
                    set(terminal, follow);
                }
            }
            Follow::New(follow) => self.new_follow = follow,
        }
    }

    /// Folder-following options for the app's menu, for the active terminal.
    pub fn follow_menu(&mut self, ui: &mut Ui) {
        let state = self.follow_state();
        let active = match self.dock.find_active_focused() {
            Some((_, terminal)) => Some((terminal.id, terminal.follow)),
            None => self
                .dock
                .iter_all_tabs()
                .next()
                .map(|(_, t)| (t.id, t.follow)),
        };
        let Some((id, follow)) = active else {
            return;
        };
        if let Some(change) = terminal::follow_menu(ui, follow, state) {
            self.set_follow(id, change);
        }
    }

    /// The focused terminal's id, if one has keyboard focus.
    fn focused_terminal(&self, ctx: &Context) -> Option<u64> {
        let focused = ctx.memory(|m| m.focused())?;
        self.dock
            .iter_all_tabs()
            .find(|(_, t)| t.widget_id() == focused)
            .map(|(_, t)| t.id)
    }

    /// `folder` is the focused browser tab's folder: new terminals start in
    /// it, and following ones keep to it.
    pub fn ui(&mut self, ui: &mut Ui, folder: Option<&Path>) -> PanelResponse {
        let mut actions = self.shortcuts(ui.ctx());
        let mut response = PanelResponse::default();
        let single = self.dock.iter_all_tabs().nth(1).is_none();
        let mut viewer = Viewer {
            folder,
            follow: self.follow_state(),
            actions: &mut actions,
            navigate: &mut response.navigate,
        };
        DockArea::new(&mut self.dock)
            .id(Id::new("terminal_dock"))
            .style(Style::from_egui(ui.style()))
            .show_add_buttons(true)
            .show_leaf_collapse_buttons(false)
            .show_leaf_close_all_buttons(false)
            // One terminal can't be torn off into a split with itself.
            .draggable_tabs(!single)
            .show_inside(ui, &mut viewer);

        for action in actions {
            match action {
                Action::Open { from, split, leaf } => self.open(from, split, leaf),
                Action::Close(id) => {
                    if let Some(path) = self.dock.find_tab_from(|t| t.id == id) {
                        self.dock.remove_tab(path);
                    }
                    self.focus();
                }
                Action::Follow { id, change } => self.set_follow(id, change),
            }
        }
        response.close = self.dock.iter_all_tabs().next().is_none();
        response
    }

    /// Panel shortcuts, taken before the focused terminal sees the keys.
    fn shortcuts(&mut self, ctx: &Context) -> Vec<Action> {
        let Some(id) = self.focused_terminal(ctx) else {
            return Vec::new();
        };
        let ctrl_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let all = self.follow_state().all;
        let follow = self
            .dock
            .iter_all_tabs()
            .any(|(_, t)| t.id == id && t.follow);
        let mut actions = Vec::new();
        ctx.input_mut(|i| {
            // Extra modifiers don't stop a match, so the Alt one goes first.
            let change = if i.consume_key(ctrl_shift | Modifiers::ALT, Key::L) {
                Some(Follow::All(!all))
            } else if i.consume_key(ctrl_shift, Key::L) {
                Some(Follow::This(!follow))
            } else {
                None
            };
            if let Some(change) = change {
                actions.push(Action::Follow { id, change });
            }
            let open = |split| Action::Open {
                from: Some(id),
                split,
                leaf: None,
            };
            if i.consume_key(ctrl_shift, Key::T) {
                actions.push(open(None));
            }
            if i.consume_key(ctrl_shift, Key::E) {
                actions.push(open(Some(Split::Right)));
            }
            if i.consume_key(ctrl_shift, Key::O) {
                actions.push(open(Some(Split::Below)));
            }
            if i.consume_key(ctrl_shift, Key::W) {
                actions.push(Action::Close(id));
            }
        });
        actions
    }

    /// Runs a program in a new terminal tab, and focuses it.
    pub fn run(&mut self, command: crate::open_with::Launch) {
        let terminal = Terminal::with_command(self.next_id, command);
        self.next_id += 1;
        self.dock.push_to_focused_leaf(terminal);
        self.focus();
    }

    fn open(&mut self, from: Option<u64>, split: Option<Split>, leaf: Option<NodePath>) {
        let terminal = Terminal::new(self.next_id, self.new_follow);
        self.next_id += 1;
        let from = from
            .and_then(|id| self.dock.find_tab_from(|t| t.id == id))
            .map(|path| path.node_path());
        match (split, leaf.or(from)) {
            (Some(split), Some(node)) => {
                self.dock.split(node, split, 0.5, Node::leaf(terminal));
            }
            (None, Some(node)) => {
                self.dock.set_focused_node_and_surface(node);
                self.dock.push_to_focused_leaf(terminal);
            }
            (_, None) => self.dock.push_to_focused_leaf(terminal),
        }
    }
}

struct Viewer<'a> {
    folder: Option<&'a Path>,
    follow: FollowState,
    actions: &'a mut Vec<Action>,
    navigate: &'a mut Option<PathBuf>,
}

impl TabViewer for Viewer<'_> {
    type Tab = Terminal;

    fn title(&mut self, tab: &mut Terminal) -> WidgetText {
        let title = tab.title().unwrap_or("Terminal");
        if tab.follow {
            format!("{} {title}", icon::LINK_SIMPLE).into()
        } else {
            title.into()
        }
    }

    fn id(&mut self, tab: &mut Terminal) -> Id {
        Id::new(("terminal-tab", tab.id))
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut Terminal) {
        let TerminalResponse {
            navigate,
            close,
            open,
            follow,
        } = tab.ui(ui, self.folder, self.follow);
        if navigate.is_some() {
            *self.navigate = navigate;
        }
        if close {
            self.actions.push(Action::Close(tab.id));
        }
        if let Some(split) = open {
            self.actions.push(Action::Open {
                from: Some(tab.id),
                split,
                leaf: None,
            });
        }
        if let Some(change) = follow {
            self.actions.push(Action::Follow { id: tab.id, change });
        }
    }

    fn context_menu(&mut self, ui: &mut Ui, tab: &mut Terminal, _path: NodePath) {
        let mut response = TerminalResponse::default();
        terminal::open_menu(ui, &mut response);
        if let Some(split) = response.open {
            self.actions.push(Action::Open {
                from: Some(tab.id),
                split,
                leaf: None,
            });
        }
        ui.separator();
        if let Some(change) = terminal::follow_menu(ui, tab.follow, self.follow) {
            self.actions.push(Action::Follow { id: tab.id, change });
        }
    }

    fn on_tab_button(&mut self, tab: &mut Terminal, response: &egui::Response) {
        // Clicking a tab moves keyboard focus into its terminal.
        if response.clicked() {
            tab.request_focus();
        }
    }

    fn on_add(&mut self, path: NodePath) {
        self.actions.push(Action::Open {
            from: None,
            split: None,
            leaf: Some(path),
        });
    }

    fn scroll_bars(&self, _tab: &Terminal) -> [bool; 2] {
        [false, false]
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use egui::{Event, Pos2, Rect, vec2};

    use super::*;

    fn frame(ctx: &Context, panel: &mut TerminalPanel, events: Vec<Event>) -> PanelResponse {
        let input = egui::RawInput {
            events,
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0))),
            ..Default::default()
        };
        let mut response = PanelResponse::default();
        let mut output = ctx.run_ui(input, |ui| response = panel.ui(ui, None));
        output.textures_delta.clear();
        response
    }

    fn key(key: Key) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            // What egui-winit reports for Ctrl+Shift on Linux.
            modifiers: Modifiers::CTRL | Modifiers::COMMAND | Modifiers::SHIFT,
        }
    }

    #[test]
    fn opens_splits_tabs_and_closes() {
        let ctx = Context::default();
        let mut panel = TerminalPanel::default();
        frame(&ctx, &mut panel, Vec::new());
        assert!(panel.has_focus(&ctx));
        let count = |panel: &TerminalPanel| panel.dock.iter_all_tabs().count();
        let leaves = |panel: &TerminalPanel| {
            panel
                .dock
                .main_surface()
                .iter()
                .filter(|n| n.is_leaf())
                .count()
        };

        // Split right: a second leaf, and the new terminal gets the keys.
        frame(&ctx, &mut panel, vec![key(Key::E)]);
        frame(&ctx, &mut panel, Vec::new());
        assert_eq!((count(&panel), leaves(&panel)), (2, 2));
        assert_eq!(panel.focused_terminal(&ctx), Some(2));

        // A new tab joins the focused leaf.
        frame(&ctx, &mut panel, vec![key(Key::T)]);
        frame(&ctx, &mut panel, Vec::new());
        assert_eq!((count(&panel), leaves(&panel)), (3, 2));
        assert_eq!(panel.focused_terminal(&ctx), Some(3));
        // Only the first terminal follows the folder.
        let follow: Vec<_> = panel.dock.iter_all_tabs().map(|(_, t)| t.follow).collect();
        assert_eq!(follow.iter().filter(|f| **f).count(), 1);

        // Closing them one by one; the last one closes the panel.
        for left in [2, 1] {
            frame(&ctx, &mut panel, vec![key(Key::W)]);
            frame(&ctx, &mut panel, Vec::new());
            assert_eq!(count(&panel), left);
            assert!(panel.has_focus(&ctx), "focus moves to a remaining terminal");
        }
        let response = frame(&ctx, &mut panel, vec![key(Key::W)]);
        assert!(response.close);
    }

    #[test]
    fn follow_shortcuts_and_options() {
        let ctx = Context::default();
        let mut panel = TerminalPanel::default();
        frame(&ctx, &mut panel, Vec::new());
        frame(&ctx, &mut panel, vec![key(Key::E)]);
        frame(&ctx, &mut panel, Vec::new());
        let follow = |panel: &TerminalPanel| -> Vec<bool> {
            panel.dock.iter_all_tabs().map(|(_, t)| t.follow).collect()
        };
        assert_eq!(follow(&panel), [true, false]);

        // Ctrl+Shift+L toggles the focused (second) terminal.
        frame(&ctx, &mut panel, vec![key(Key::L)]);
        assert_eq!(follow(&panel), [true, true]);
        frame(&ctx, &mut panel, vec![key(Key::L)]);
        assert_eq!(follow(&panel), [true, false]);

        // Ctrl+Shift+Alt+L: all follow unless all already do, then none.
        let all = Event::Key {
            key: Key::L,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL | Modifiers::COMMAND | Modifiers::SHIFT | Modifiers::ALT,
        };
        frame(&ctx, &mut panel, vec![all.clone()]);
        assert_eq!(follow(&panel), [true, true]);
        frame(&ctx, &mut panel, vec![all]);
        assert_eq!(follow(&panel), [false, false]);

        // "New terminals follow" applies to terminals opened afterwards, and
        // outlives a reset.
        panel.set_follow(0, Follow::New(true));
        frame(&ctx, &mut panel, vec![key(Key::T)]);
        assert_eq!(follow(&panel), [false, false, true]);
        panel.reset();
        assert!(panel.new_follow);
    }

    #[test]
    fn layout_survives_restart() {
        let ctx = Context::default();
        let mut panel = TerminalPanel::default();
        frame(&ctx, &mut panel, Vec::new());
        frame(&ctx, &mut panel, vec![key(Key::E)]);
        frame(&ctx, &mut panel, Vec::new());
        assert_eq!(panel.focused_terminal(&ctx), Some(2));

        // cd in the new, non-following terminal.
        let enter = Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        frame(&ctx, &mut panel, vec![Event::Text("cd /".into()), enter]);
        let dir = |panel: &TerminalPanel, id| {
            let (_, t) = panel
                .dock
                .iter_all_tabs()
                .find(|(_, t)| t.id == id)
                .unwrap();
            t.dir.clone()
        };
        let start = web_time::Instant::now();
        while dir(&panel, 2) != Some("/".into()) {
            assert!(start.elapsed().as_secs() < 10, "shell didn't cd");
            frame(&ctx, &mut panel, Vec::new());
            panel.remember_dirs();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let saved = ron::ser::to_string(&panel).unwrap();
        drop(panel);
        let mut restored: TerminalPanel = ron::from_str(&saved).unwrap();
        let tabs: Vec<_> = restored
            .dock
            .iter_all_tabs()
            .map(|(_, t)| (t.id, t.follow))
            .collect();
        assert_eq!(tabs, [(1, true), (2, false)]);
        let leaves = restored
            .dock
            .main_surface()
            .iter()
            .filter(|n| n.is_leaf())
            .count();
        assert_eq!(leaves, 2);
        assert_eq!(restored.next_id, 3);

        // The restored shell starts back in /, not in the tab's folder.
        let tmp = std::env::temp_dir();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0))),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            restored.ui(ui, Some(&tmp));
        });
        output.textures_delta.clear();
        restored.remember_dirs();
        assert_eq!(dir(&restored, 2), Some("/".into()));
        assert_eq!(
            dir(&restored, 1),
            Some(std::fs::canonicalize(&tmp).unwrap())
        );
    }
}
