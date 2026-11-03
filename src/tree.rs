//! Lazily loaded folder tree for the sidebar. It follows the focused tab by
//! expanding the ancestors of its folder.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use egui::{ScrollArea, Ui};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use crate::{
    dnd::{self, DropTarget},
    fs,
    jobs::JobKind,
    places::Place,
    widgets::{self, LEADING_WIDTH, ListRow},
};

const INDENT: f32 = 14.0;

struct Node {
    path: PathBuf,
    name: String,
    hidden: bool,
}

/// A row of the flattened, visible part of the tree.
struct Row<'a> {
    depth: usize,
    path: PathBuf,
    name: String,
    icon: &'a str,
    hidden: bool,
    expanded: bool,
    /// Known to have no subfolders.
    leaf: bool,
}

pub enum TreeAction {
    Navigate(PathBuf),
    OpenInNewTab(PathBuf),
    /// Files were dropped onto a folder.
    Drop(Box<JobKind>),
    Properties(PathBuf),
}

#[derive(Default, Serialize, Deserialize)]
pub struct FolderTree {
    expanded: HashSet<PathBuf>,
    #[serde(skip)]
    children: HashMap<PathBuf, Arc<Vec<Node>>>,
    #[serde(skip)]
    synced_to: Option<PathBuf>,
    #[serde(skip)]
    scroll_to_current: bool,
    /// Picking a folder in a dialog: no tabs to open, so no right-click menu.
    #[serde(skip)]
    pub picker: bool,
}

impl FolderTree {
    pub fn picker() -> Self {
        Self {
            picker: true,
            ..Self::default()
        }
    }

    pub fn invalidate(&mut self) {
        self.children.clear();
    }

    pub fn dirs_changed(&mut self, dirs: &HashSet<PathBuf>) {
        self.children.retain(|dir, _| !dirs.contains(dir));
    }

    fn children(&mut self, dir: &Path) -> Arc<Vec<Node>> {
        self.children
            .entry(dir.to_path_buf())
            .or_insert_with(|| {
                let mut entries = fs::read_dir(dir).unwrap_or_default();
                entries.retain(|e| e.is_dir);
                fs::sort_entries(&mut entries, fs::Sort::default());
                Arc::new(
                    entries
                        .into_iter()
                        .map(|e| Node {
                            path: e.path,
                            name: e.name,
                            hidden: e.hidden,
                        })
                        .collect(),
                )
            })
            .clone()
    }

    /// Expands the ancestors of `current` under the most specific root containing it.
    fn sync(&mut self, current: &Path, roots: &[&Place]) {
        if self.synced_to.as_deref() == Some(current) {
            return;
        }
        self.synced_to = Some(current.to_path_buf());
        self.scroll_to_current = true;
        let Some(root) = roots
            .iter()
            .map(|r| r.path.as_path())
            .filter(|r| current.starts_with(r))
            .max_by_key(|r| r.components().count())
        else {
            return;
        };
        for ancestor in current.ancestors().skip(1) {
            if !ancestor.starts_with(root) {
                break;
            }
            self.expanded.insert(ancestor.to_path_buf());
        }
    }

    fn flatten<'a>(
        &mut self,
        roots: &[&'a Place],
        show_hidden: bool,
        current: Option<&Path>,
    ) -> Vec<Row<'a>> {
        let mut rows = Vec::new();
        for root in roots {
            self.push_rows(
                &mut rows,
                &root.path,
                root.label.clone(),
                root.icon,
                false,
                0,
                show_hidden,
                current,
            );
        }
        rows
    }

    #[allow(clippy::too_many_arguments)]
    fn push_rows<'a>(
        &mut self,
        rows: &mut Vec<Row<'a>>,
        path: &Path,
        name: String,
        icon: &'a str,
        hidden: bool,
        depth: usize,
        show_hidden: bool,
        current: Option<&Path>,
    ) {
        let expanded = self.expanded.contains(path);
        let leaf = self.children.get(path).is_some_and(|c| c.is_empty());
        rows.push(Row {
            depth,
            path: path.to_path_buf(),
            name,
            icon,
            hidden,
            expanded,
            leaf,
        });
        if !expanded {
            return;
        }
        let children = self.children(path);
        if children.is_empty() {
            rows.last_mut().expect("just pushed").leaf = true;
        }
        for child in children.iter() {
            // Hidden folders still show when the current folder is inside them.
            let on_current_path = current.is_some_and(|c| c.starts_with(&child.path));
            if child.hidden && !show_hidden && !on_current_path {
                continue;
            }
            let glyph = if self.expanded.contains(&child.path) {
                icon::FOLDER_OPEN
            } else {
                icon::FOLDER
            };
            self.push_rows(
                rows,
                &child.path,
                child.name.clone(),
                glyph,
                child.hidden,
                depth + 1,
                show_hidden,
                current,
            );
        }
    }

    pub fn ui(
        &mut self,
        ui: &mut Ui,
        roots: &[&Place],
        current: Option<&Path>,
        show_hidden: bool,
    ) -> Option<TreeAction> {
        if let Some(current) = current {
            self.sync(current, roots);
        }
        let rows = self.flatten(roots, show_hidden, current);
        let row_height = widgets::row_height(ui);
        let picker = self.picker;
        let mut action = None;
        let mut toggle = None;

        ui.spacing_mut().item_spacing.y = 0.0;
        let folder_color = crate::theme::colors(ui.ctx()).icon(crate::theme::IconKind::Folder);
        let output = ScrollArea::vertical()
            .scroll_source(crate::touch::scroll_source(ui.ctx()))
            .id_salt("folder_tree")
            .auto_shrink(false)
            .show_rows(ui, row_height, rows.len(), |ui, range| {
                for row in &rows[range] {
                    let indent = row.depth as f32 * INDENT;
                    let caret = match (row.leaf, row.expanded) {
                        (true, _) => None,
                        (false, true) => Some(icon::CARET_DOWN),
                        (false, false) => Some(icon::CARET_RIGHT),
                    };
                    let response = ListRow {
                        icon: row.icon,
                        name: &row.name,
                        selected: current == Some(row.path.as_path()),
                        weak: row.hidden,
                        icon_color: folder_color,
                        name_color: None,
                        indent,
                        leading: Some(caret.unwrap_or("")),
                        trailing: None,
                    }
                    .show(ui)
                    .on_hover_text(row.path.display().to_string());
                    dnd::source(&response, || vec![row.path.clone()]);
                    if let Some(job) = dnd::target(&response, DropTarget::Folder(row.path.clone()))
                    {
                        action = Some(TreeAction::Drop(Box::new(job)));
                    }

                    let on_caret = response.interact_pointer_pos().is_some_and(|pos| {
                        let caret_left = response.rect.left() + 4.0 + indent;
                        (caret_left..caret_left + LEADING_WIDTH).contains(&pos.x)
                    });
                    if response.clicked() && on_caret || response.double_clicked() {
                        toggle = Some(row.path.clone());
                    } else if response.clicked() {
                        action = Some(TreeAction::Navigate(row.path.clone()));
                    } else if response.middle_clicked() {
                        action = Some(TreeAction::OpenInNewTab(row.path.clone()));
                    }
                    if !picker {
                        response.context_menu(|ui| {
                            if ui
                                .button(format!("{}  Open in new tab", icon::PLUS))
                                .clicked()
                            {
                                action = Some(TreeAction::OpenInNewTab(row.path.clone()));
                                ui.close();
                            }
                            if ui.button(format!("{}  Properties", icon::INFO)).clicked() {
                                action = Some(TreeAction::Properties(row.path.clone()));
                                ui.close();
                            }
                        });
                    }
                }
            });

        if std::mem::take(&mut self.scroll_to_current)
            && let Some(index) = current.and_then(|c| rows.iter().rposition(|r| r.path == c))
        {
            widgets::ensure_row_visible(ui, &output, index, row_height);
        }
        if let Some(path) = toggle
            && !self.expanded.remove(&path)
        {
            self.expanded.insert(path);
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syncs_to_current_folder() {
        let root = std::env::temp_dir().join(format!("egui-files-tree-{}", std::process::id()));
        let _ = disk::remove_dir_all(&root);
        disk::create_dir_all(root.join("x/y/z")).unwrap();
        let place = Place {
            label: "Root".to_owned(),
            icon: icon::HARD_DRIVE,
            path: root.clone(),
            ejectable: false,
            media: Some(0),
            space: None,
        };
        let current = root.join("x/y/z");

        let mut tree = FolderTree::default();
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let mut output = ctx.run_ui(Default::default(), |ui| {
                tree.ui(ui, &[&place], Some(&current), false);
            });
            output.textures_delta.clear();
        }
        for dir in [root.clone(), root.join("x"), root.join("x/y")] {
            assert!(tree.expanded.contains(&dir), "{} expanded", dir.display());
        }
        assert!(!tree.expanded.contains(&current));
        assert_eq!(tree.children[&root.join("x/y")].len(), 1);

        disk::remove_dir_all(root).unwrap();
    }
}
