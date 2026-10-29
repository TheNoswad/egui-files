//! Large-icon grid view with optional image thumbnails.

use egui::{Align, Color32, FontId, Rect, ScrollArea, Sense, Ui, pos2, text::LayoutJob, vec2};

use crate::{
    images,
    tab::{Action, BrowserTab, Shared},
};

pub fn show(
    ui: &mut Ui,
    tab: &BrowserTab,
    visible: &[usize],
    shared: &mut Shared,
    actions: &mut Vec<Action>,
) {
    let icon_size = shared.icon_size;
    let cell = vec2(icon_size + 32.0, icon_size + 40.0);
    ScrollArea::vertical()
        .id_salt(("grid", tab.id))
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                for (index, &entry_index) in visible.iter().enumerate() {
                    let entry = &tab.entries()[entry_index];
                    let selected = tab.selection.contains(&entry.path);
                    let (rect, response) = ui.allocate_exact_size(cell, Sense::click());
                    if tab.scroll_to == Some(index) {
                        response.scroll_to_me(None);
                    }

                    // Only paint (and request thumbnails for) cells on screen.
                    if ui.is_rect_visible(rect) {
                        let visuals = ui.visuals();
                        if selected {
                            ui.painter()
                                .rect_filled(rect, 4.0, visuals.selection.bg_fill);
                        } else if response.hovered() {
                            ui.painter().rect_filled(
                                rect,
                                4.0,
                                visuals.widgets.hovered.weak_bg_fill,
                            );
                        }
                        let text_color = if selected {
                            visuals.selection.stroke.color
                        } else {
                            visuals.text_color()
                        };

                        let icon_rect = Rect::from_center_size(
                            rect.center_top() + vec2(0.0, 6.0 + icon_size / 2.0),
                            vec2(icon_size, icon_size),
                        );
                        let thumbnail = (shared.show_thumbnails && !entry.is_dir)
                            .then(|| {
                                shared.images.get_ready(
                                    &entry.path,
                                    entry.modified,
                                    images::THUMBNAIL_SIZE,
                                )
                            })
                            .flatten();
                        if let Some(thumbnail) = thumbnail {
                            let size = thumbnail.texture.size_vec2();
                            let scale =
                                (icon_rect.width() / size.x).min(icon_rect.height() / size.y);
                            ui.painter().image(
                                thumbnail.texture.id(),
                                Rect::from_center_size(icon_rect.center(), size * scale),
                                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        } else {
                            ui.painter().text(
                                icon_rect.center(),
                                egui::Align2::CENTER_CENTER,
                                entry.icon(),
                                FontId::proportional(icon_size * 0.75),
                                text_color,
                            );
                        }

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
                        ui.painter().galley(
                            pos2(rect.center().x, icon_rect.bottom() + 4.0),
                            galley,
                            text_color,
                        );
                    }

                    let response = response.on_hover_text(&entry.name);
                    super::item_interaction(&response, index, entry, selected, actions);
                }
            });
        });
}
