//! Large-icon grid view.

use egui::{Align, FontId, ScrollArea, Sense, Ui, text::LayoutJob, vec2};

use crate::tab::{Action, BrowserTab};

const CELL: egui::Vec2 = vec2(96.0, 88.0);

pub fn show(ui: &mut Ui, tab: &BrowserTab, visible: &[usize], actions: &mut Vec<Action>) {
    ScrollArea::vertical()
        .id_salt(("grid", tab.id))
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                for (index, &entry_index) in visible.iter().enumerate() {
                    let entry = &tab.entries()[entry_index];
                    let selected = tab.selection.contains(&entry.path);
                    let (rect, response) = ui.allocate_exact_size(CELL, Sense::click());
                    if tab.scroll_to == Some(index) {
                        response.scroll_to_me(None);
                    }

                    let visuals = ui.visuals();
                    if selected {
                        ui.painter()
                            .rect_filled(rect, 4.0, visuals.selection.bg_fill);
                    } else if response.hovered() {
                        ui.painter()
                            .rect_filled(rect, 4.0, visuals.widgets.hovered.weak_bg_fill);
                    }
                    let text_color = if selected {
                        visuals.selection.stroke.color
                    } else {
                        visuals.text_color()
                    };

                    ui.painter().text(
                        rect.center_top() + vec2(0.0, 6.0),
                        egui::Align2::CENTER_TOP,
                        entry.icon(),
                        FontId::proportional(36.0),
                        text_color,
                    );
                    let mut job = LayoutJob::simple(
                        entry.name.clone(),
                        FontId::proportional(12.0),
                        text_color,
                        rect.width() - 8.0,
                    );
                    job.wrap.max_rows = 2;
                    job.wrap.break_anywhere = true;
                    job.halign = Align::Center;
                    let galley = ui.painter().layout_job(job);
                    ui.painter()
                        .galley(rect.center_top() + vec2(0.0, 50.0), galley, text_color);

                    let response = response.on_hover_text(&entry.name);
                    super::item_interaction(&response, index, entry, selected, actions);
                }
            });
        });
}
