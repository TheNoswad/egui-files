//! Laying the app out as a scene (see crate::scene), and taking its
//! screenshot.

use std::path::{Path, PathBuf};

use egui_dock::{DockState, Node, NodePath, Split};

use super::FilesApp;
use crate::{
    archive::ArchiveTab,
    dock_tab::{DockTab, Pane},
    places,
    scene::Scene,
    tab::BrowserTab,
    views::ViewMode,
};

impl FilesApp {
    /// Lays out the tabs and panes of `scene`, in place of the saved ones.
    pub(super) fn set_scene(&mut self, scene: Scene) {
        let home = places::home_dir();
        match scene {
            Scene::Markdown => {
                let mut tab = self.scene_tab(&home, ViewMode::Details);
                tab.reveal(home.join("README.md"));
                self.lay_out(tab, None, &[Pane::Places, Pane::Tree, Pane::Preview]);
            }
            Scene::Split => {
                let left = self.scene_tab(&home.join("Projects/egui-files"), ViewMode::Details);
                let right = self.scene_tab(&home.join("Documents"), ViewMode::Details);
                let right = DockTab::Browser(right);
                self.lay_out(
                    left,
                    Some(right),
                    &[Pane::Places, Pane::Tree, Pane::Terminal],
                );
                self.terminal.focus();
            }
            Scene::Archive => {
                let pictures = self.scene_tab(&home.join("Pictures"), ViewMode::Grid);
                let archive = home.join("Downloads/egui-files-source.tar.gz");
                let archive = DockTab::Archive(ArchiveTab::new(self.next_id(), archive));
                self.lay_out(pictures, Some(archive), &[Pane::Places, Pane::Tree]);
            }
            Scene::Phone => {
                let pictures = self.scene_tab(&home.join("Pictures"), ViewMode::Grid);
                self.lay_out(pictures, None, &[]);
            }
        }
    }

    fn scene_tab(&mut self, path: &Path, view: ViewMode) -> BrowserTab {
        let mut tab = BrowserTab::new(self.next_id(), PathBuf::from(path));
        tab.view = view;
        tab
    }

    /// `main` with `beside` to its right, and `panes` docked as they are by
    /// default, once the dock's size is known. `main` has the focus.
    fn lay_out(&mut self, main: BrowserTab, beside: Option<DockTab>, panes: &[Pane]) {
        let id = main.id;
        self.dock = DockState::new(vec![DockTab::Browser(main)]);
        if let Some(beside) = beside {
            self.dock
                .split(NodePath::MAIN_ROOT, Split::Right, 0.5, Node::leaf(beside));
        }
        self.pending_panes = panes.to_vec();
        self.focus_tab(id);
    }

    /// Takes a staged scene's screenshot when it's time, and quits once it's
    /// saved.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn shoot(&mut self, ctx: &egui::Context) {
        let Some(staged) = crate::scene::staged() else {
            return;
        };
        let (Some(path), Some(shooting)) = (&staged.shot, &mut self.shooting) else {
            return;
        };
        // Keep going without input, to see the time pass.
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        if shooting.shoot_now() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            match crate::scene::save(&image, path) {
                Ok(()) => eprintln!("saved {}", path.display()),
                Err(err) => {
                    eprintln!("couldn't save {}: {err}", path.display());
                    std::process::exit(1);
                }
            }
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// A staged scene is shown without the mouse, so nothing is hovered, and
    /// has what it types typed.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn stage_input(&mut self, raw_input: &mut egui::RawInput) {
        let Some(staged) = crate::scene::staged() else {
            return;
        };
        // As if in front: a virtual X server has no window manager to say so,
        // and egui keeps no widget focused in a window without it.
        raw_input.focused = true;
        raw_input.events.retain(|e| {
            !matches!(
                e,
                egui::Event::PointerMoved(_)
                    | egui::Event::MouseMoved(_)
                    | egui::Event::PointerButton { .. }
                    | egui::Event::PointerGone
                    | egui::Event::MouseWheel { .. }
            )
        });
        if let Some(shooting) = &mut self.shooting
            && let Some(text) = shooting.type_now(staged.scene)
        {
            // The terminal's the only thing a scene types into.
            self.terminal.focus();
            raw_input.events.push(egui::Event::Text(text.to_owned()));
            for pressed in [true, false] {
                raw_input.events.push(egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                });
            }
        }
    }
}
