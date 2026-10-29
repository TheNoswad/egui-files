//! The terminal panel: terminals in tabs and splits, in a dock of their own.

use std::path::{Path, PathBuf};

use egui::{Context, Id, Key, Modifiers, Ui, WidgetText};
use egui_dock::{DockArea, DockState, Node, NodePath, Split, Style, TabViewer};
use egui_phosphor::regular as icon;

use crate::terminal::{self, Terminal, TerminalResponse};

/// What the app should do after the panel was drawn.
#[derive(Default)]
pub struct PanelResponse {
    /// A terminal's shell changed folder; show it in the focused tab.
    pub navigate: Option<PathBuf>,
    /// The last terminal closed.
    pub close: bool,
}

pub struct TerminalPanel {
    dock: DockState<Terminal>,
    next_id: u64,
}

impl Default for TerminalPanel {
    fn default() -> Self {
        // The first terminal follows the focused folder; ones opened later
        // stay where they are unless asked to.
        Self {
            dock: DockState::new(vec![Terminal::new(1, true)]),
            next_id: 2,
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

    /// Ends every shell; the next `ui` call starts a single new one.
    pub fn reset(&mut self) {
        *self = Self::default();
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
        let mut actions = Vec::new();
        ctx.input_mut(|i| {
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

    fn open(&mut self, from: Option<u64>, split: Option<Split>, leaf: Option<NodePath>) {
        let terminal = Terminal::new(self.next_id, false);
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
        } = tab.ui(ui, self.folder);
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
        if ui
            .checkbox(&mut tab.follow, "Follow folder")
            .on_hover_text("Keep the shell in the focused tab's folder, and the tab in the shell's")
            .changed()
        {
            ui.close();
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
}
