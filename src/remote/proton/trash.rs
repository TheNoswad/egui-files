//! The Drive's trash, in a window: what's been removed from it can be put
//! back from here, or deleted for good.

use std::{
    collections::HashSet,
    sync::{
        Mutex, MutexGuard, PoisonError,
        mpsc::{self, Receiver},
    },
};

use disk::proton::Node;
use egui::{Context, Id, RichText};
use egui_phosphor::regular as icon;
use proton_sdk::ids::NodeUid;

use super::{client, first_failure, parse_uid, wait};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Confirm {
    Selected,
    Everything,
}

#[derive(Default)]
struct Window {
    /// `None` until it's been listed.
    items: Option<Vec<Node>>,
    selected: HashSet<String>,
    confirm: Option<Confirm>,
    error: Option<String>,
    /// How many items are in the trash that couldn't be read.
    unreadable: usize,
    task: Option<Receiver<Result<Listed, String>>>,
}

/// What's in the trash, and how many items in it couldn't be read.
type Listed = (Vec<Node>, usize);

static WINDOW: Mutex<Option<Window>> = Mutex::new(None);

fn window() -> MutexGuard<'static, Option<Window>> {
    WINDOW.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Does `work` in the background, then lists the trash again.
fn start(window: &mut Window, work: impl FnOnce() -> Result<(), String> + Send + 'static) {
    let (send, receive) = mpsc::channel();
    crate::background::spawn("change Proton Drive's trash", move || {
        let result = work();
        // What changed shows up in folders, and in how full the Drive is.
        disk::proton::folder_changed(None);
        super::refresh_space();
        let _ = send.send(result.and_then(|()| list()));
        crate::remote::repaint();
    });
    window.task = Some(receive);
    window.error = None;
    window.confirm = None;
}

fn list() -> Result<Listed, String> {
    let client = client()?;
    let (total, nodes) = wait(async {
        let uids = client.enumerate_trash_node_uids().await?;
        Ok::<_, proton_sdk::error::ProtonError>((uids.len(), client.enumerate_nodes(&uids).await?))
    })
    .map_err(|err| super::describe(&err))?;
    // The SDK leaves out what it can't decrypt, and says so only in its log.
    let unreadable = total.saturating_sub(nodes.len());
    let mut nodes: Vec<Node> = nodes.into_iter().map(super::node_of).collect();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((nodes, unreadable))
}

fn uids(ids: &[String]) -> Result<Vec<NodeUid>, String> {
    ids.iter()
        .map(|id| parse_uid(id).map_err(|err| err.to_string()))
        .collect()
}

/// Opens the window, unless it's open already.
pub fn open() {
    let mut guard = window();
    if guard.is_none() {
        let mut new = Window::default();
        start(&mut new, || Ok(()));
        *guard = Some(new);
    }
}

fn restore(ids: Vec<String>) -> Result<(), String> {
    let (client, uids) = (client()?, uids(&ids)?);
    wait(client.restore_nodes(&uids))
        .and_then(first_failure)
        .map_err(|err| super::describe(&err))
}

fn delete(ids: Vec<String>) -> Result<(), String> {
    let (client, uids) = (client()?, uids(&ids)?);
    wait(client.delete_nodes(&uids))
        .and_then(first_failure)
        .map_err(|err| super::describe(&err))
}

fn empty() -> Result<(), String> {
    let client = client()?;
    wait(client.empty_trash()).map_err(|err| super::describe(&err))
}

/// Shows the window if it's open.
pub fn ui(ctx: &Context) {
    let mut guard = window();
    let Some(window) = guard.as_mut() else { return };
    if let Some(Ok(result)) = window.task.as_ref().map(Receiver::try_recv) {
        window.task = None;
        match result {
            Ok((items, unreadable)) => {
                window.unreadable = unreadable;
                window
                    .selected
                    .retain(|id| items.iter().any(|item| item.id == *id));
                window.items = Some(items);
            }
            Err(why) => window.error = Some(why),
        }
    }
    let busy = window.task.is_some();
    let mut close = false;
    let modal = egui::Modal::new(Id::new("proton_trash")).show(ctx, |ui| {
        ui.set_max_width(crate::widgets::modal_width(ui, 520.0));
        ui.heading(format!("{} Proton Drive trash", icon::TRASH));
        ui.add_space(6.0);
        match &window.items {
            None if busy => {
                ui.spinner();
            }
            None => {}
            Some(items) if items.is_empty() => {
                ui.label(RichText::new("The trash is empty.").weak());
            }
            Some(items) => {
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for item in items {
                            let mut on = window.selected.contains(&item.id);
                            let glyph = if item.is_dir {
                                icon::FOLDER
                            } else {
                                icon::FILE
                            };
                            let label = format!("{glyph} {}", item.name);
                            if ui
                                .add_enabled(!busy, egui::Checkbox::new(&mut on, label))
                                .changed()
                            {
                                if on {
                                    window.selected.insert(item.id.clone());
                                } else {
                                    window.selected.remove(&item.id);
                                }
                            }
                        }
                    });
            }
        }
        if window.unreadable > 0 {
            ui.add_space(4.0);
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!(
                    "{} more in the trash couldn't be read here.",
                    window.unreadable
                ),
            );
        }
        if let Some(why) = &window.error {
            ui.add_space(4.0);
            ui.colored_label(ui.visuals().error_fg_color, why);
        }
        ui.add_space(8.0);
        let selected: Vec<String> = window.selected.iter().cloned().collect();
        let has_items = window.items.as_ref().is_some_and(|items| !items.is_empty());
        if let Some(confirm) = window.confirm {
            let question = match confirm {
                Confirm::Selected => format!(
                    "Delete {} permanently? This can't be undone.",
                    if selected.len() == 1 {
                        "1 item".to_owned()
                    } else {
                        format!("{} items", selected.len())
                    }
                ),
                Confirm::Everything => {
                    "Delete everything in the trash permanently? This can't be undone.".to_owned()
                }
            };
            ui.colored_label(ui.visuals().warn_fg_color, question);
            ui.horizontal(|ui| {
                if ui
                    .button(format!("{} Delete permanently", icon::X))
                    .clicked()
                {
                    match confirm {
                        Confirm::Selected => start(window, move || delete(selected)),
                        Confirm::Everything => start(window, empty),
                    }
                } else if ui.button("Keep them").clicked() {
                    window.confirm = None;
                }
            });
            return;
        }
        ui.horizontal(|ui| {
            let any = !busy && !selected.is_empty();
            if ui
                .add_enabled(
                    any,
                    egui::Button::new(format!("{} Restore", icon::ARROW_U_UP_LEFT)),
                )
                .clicked()
            {
                let ids = selected.clone();
                start(window, move || restore(ids));
            }
            if ui
                .add_enabled(any, egui::Button::new(format!("{} Delete…", icon::X)))
                .clicked()
            {
                window.confirm = Some(Confirm::Selected);
            }
            if ui
                .add_enabled(!busy && has_items, egui::Button::new("Empty trash…"))
                .clicked()
            {
                window.confirm = Some(Confirm::Everything);
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Refresh"))
                .clicked()
            {
                start(window, || Ok(()));
            }
            if ui.button("Close").clicked() {
                close = true;
            }
            if busy {
                ui.spinner();
            }
        });
    });
    if close || modal.should_close() {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_with_items_and_when_asking() {
        let item = Node {
            id: "v~l".to_owned(),
            name: "old.txt".to_owned(),
            is_dir: false,
            size: 1,
            modified: 0,
        };
        for confirm in [None, Some(Confirm::Selected), Some(Confirm::Everything)] {
            *window() = Some(Window {
                items: Some(vec![item.clone()]),
                selected: HashSet::from([item.id.clone()]),
                confirm,
                ..Window::default()
            });
            let ctx = Context::default();
            let mut output = ctx.run_ui(Default::default(), |ui| super::ui(ui.ctx()));
            output.textures_delta.clear();
            assert!(window().is_some());
        }
        *window() = None;
    }
}
