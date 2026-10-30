//! The "Other application…" dialog: every app, searchable, with the ones
//! registered for the files first, and optionally a custom command.

use egui::{Button, Context, Id, Key, Modal, RichText, ScrollArea, TextEdit, Ui, vec2};
use egui_phosphor::regular as icon;

use super::{App, Custom, Prefs, Request, Subject, Target, app_icon, catalog, describe, handlers};

/// What the dialog has selected.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Picked {
    App(String),
    Custom,
}

pub struct Dialog {
    request: Request,
    subjects: Vec<Subject>,
    search: String,
    picked: Option<Picked>,
    command: String,
    terminal: bool,
    just_opened: bool,
    /// Every app, listed once the catalog is loaded.
    all: Option<Vec<App>>,
}

pub enum Outcome {
    Open,
    Closed,
    Chosen(Target),
}

/// Whether `app` matches every word of the search.
fn matches(app: &App, search: &str) -> bool {
    let haystack = format!("{} {} {}", app.name, app.comment, app.id).to_lowercase();
    search
        .to_lowercase()
        .split_whitespace()
        .all(|word| haystack.contains(word))
}

impl Dialog {
    pub fn new(request: Request) -> Self {
        Self {
            subjects: request.subjects(),
            request,
            search: String::new(),
            picked: None,
            command: String::new(),
            terminal: false,
            just_opened: true,
            all: None,
        }
    }

    pub fn into_request(self) -> Request {
        self.request
    }

    fn target(&self, apps: &[&App]) -> Option<Target> {
        match self.picked.as_ref()? {
            Picked::App(id) => apps
                .iter()
                .find(|a| a.id == *id)
                .map(|a| Target::App((*a).clone())),
            Picked::Custom => {
                let command = self.command.trim();
                (!command.is_empty()).then(|| Target::Command {
                    command: command.to_owned(),
                    terminal: self.terminal,
                })
            }
        }
    }

    pub fn show(&mut self, ctx: &Context, prefs: &mut Prefs) -> Outcome {
        let mut outcome = Outcome::Open;
        let modal = Modal::new(Id::new("open-with-dialog")).show(ctx, |ui| {
            ui.set_width(460.0);
            outcome = self.contents(ui, prefs);
        });
        if modal.should_close() && matches!(outcome, Outcome::Open) {
            outcome = Outcome::Closed;
        }
        outcome
    }

    fn contents(&mut self, ui: &mut Ui, prefs: &mut Prefs) -> Outcome {
        let ctx = ui.ctx().clone();
        ui.heading(format!("Open “{}” with", describe(&self.subjects)));
        let handlers = handlers(&ctx, &self.subjects);
        if let Some(handlers) = &handlers
            && !handlers.types.is_empty()
        {
            ui.label(RichText::new(handlers.types.join(", ")).weak());
        }
        ui.add_space(4.0);

        let search = ui.add(
            TextEdit::singleline(&mut self.search)
                .hint_text(format!("{} Search applications", icon::MAGNIFYING_GLASS))
                .desired_width(f32::INFINITY),
        );
        if std::mem::take(&mut self.just_opened) {
            search.request_focus();
        }

        if self.all.is_none() {
            self.all = catalog(&ctx).map(|c| c.all_apps(&self.subjects));
        }
        let (Some(handlers), Some(all)) = (handlers, self.all.clone()) else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Finding apps…");
            });
            return if ui.button("Cancel").clicked() {
                Outcome::Closed
            } else {
                Outcome::Open
            };
        };
        let recommended: Vec<&App> = handlers
            .default
            .iter()
            .chain(&handlers.others)
            .filter(|a| matches(a, &self.search))
            .collect();
        let rest: Vec<&App> = all
            .iter()
            .filter(|a| !recommended.iter().any(|r| r.id == a.id))
            .filter(|a| matches(a, &self.search))
            .collect();
        let shown: Vec<&App> = recommended.iter().chain(&rest).copied().collect();

        // Typing picks the first match; the arrows move through the list.
        if search.changed() {
            self.picked = shown.first().map(|a| Picked::App(a.id.clone()));
        }
        let mut scroll_to = None;
        if search.has_focus() {
            let (up, down) = ui.input_mut(|i| {
                (
                    i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
                    i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
                )
            });
            if (up || down) && !shown.is_empty() {
                let current = shown
                    .iter()
                    .position(|a| self.picked.as_ref() == Some(&Picked::App(a.id.clone())));
                let next = match (current, down) {
                    (None, _) => 0,
                    (Some(i), true) => (i + 1).min(shown.len() - 1),
                    (Some(i), false) => i.saturating_sub(1),
                };
                self.picked = Some(Picked::App(shown[next].id.clone()));
                scroll_to = Some(shown[next].id.clone());
            }
        }

        let mut activated = false;
        let list_height = if prefs.custom_commands { 260.0 } else { 340.0 };
        ui.add_space(4.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ScrollArea::vertical()
                .max_height(list_height)
                .min_scrolled_height(list_height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                        let mut section = |ui: &mut Ui, title: &str, apps: &[&App]| {
                            if apps.is_empty() {
                                return;
                            }
                            ui.label(RichText::new(title).small().strong());
                            for app in apps {
                                let picked = self.picked == Some(Picked::App(app.id.clone()));
                                let hint =
                                    if handlers.default.as_ref().is_some_and(|d| d.id == app.id) {
                                        "default"
                                    } else {
                                        ""
                                    };
                                let response = ui
                                    .add(
                                        Button::selectable(
                                            picked,
                                            (app_icon(&ctx, Some(app), 24.0), app.name.as_str()),
                                        )
                                        .right_text(RichText::new(hint).weak())
                                        .min_size(vec2(0.0, 28.0)),
                                    )
                                    .on_hover_text(&app.comment);
                                if response.clicked() {
                                    self.picked = Some(Picked::App(app.id.clone()));
                                }
                                if response.double_clicked() {
                                    activated = true;
                                }
                                if scroll_to.as_ref() == Some(&app.id) {
                                    response.scroll_to_me(None);
                                }
                            }
                            ui.add_space(6.0);
                        };
                        section(ui, "For this type", &recommended);
                        section(ui, "All applications", &rest);
                        if shown.is_empty() {
                            ui.label(RichText::new("No matching applications").weak());
                        }
                    });
                });
        });

        if prefs.custom_commands {
            ui.add_space(6.0);
            activated |= self.custom_ui(ui, prefs);
        }

        let target = self.target(&shown);
        ui.add_space(8.0);
        let mut outcome = Outcome::Open;
        ui.horizontal(|ui| {
            let open = ui
                .add_enabled(
                    target.is_some(),
                    Button::new(format!("{} Open", icon::ARROW_SQUARE_OUT)),
                )
                .clicked();
            if ui.button("Cancel").clicked() {
                outcome = Outcome::Closed;
            }
            let enter = ui.input(|i| i.key_pressed(Key::Enter));
            if (open || activated || enter)
                && let Some(target) = target.clone()
            {
                outcome = Outcome::Chosen(target);
            }
        });
        outcome
    }

    /// The custom command box and the commands used before. Returns whether
    /// one was double-clicked.
    fn custom_ui(&mut self, ui: &mut Ui, prefs: &mut Prefs) -> bool {
        ui.label(RichText::new("Custom command").small().strong());
        let edit = ui.add(
            TextEdit::singleline(&mut self.command)
                .hint_text("e.g. gimp --new-instance (files are added at the end, or at %f / %F)")
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Monospace),
        );
        if edit.changed() || edit.gained_focus() {
            self.picked = (!self.command.trim().is_empty()).then_some(Picked::Custom);
        }
        ui.checkbox(&mut self.terminal, "Run in the terminal pane");

        let mut activated = false;
        let mut forget = None;
        for (i, custom) in prefs.recent.iter().enumerate() {
            ui.horizontal(|ui| {
                let glyph = if custom.terminal {
                    icon::TERMINAL_WINDOW
                } else {
                    icon::CLOCK_COUNTER_CLOCKWISE
                };
                let picked = self.picked == Some(Picked::Custom) && self.command == custom.command;
                let response = ui.add(Button::selectable(
                    picked,
                    RichText::new(format!("{glyph} {}", custom.command)).monospace(),
                ));
                if response.clicked() || response.double_clicked() {
                    self.command = custom.command.clone();
                    self.terminal = custom.terminal;
                    self.picked = Some(Picked::Custom);
                }
                activated |= response.double_clicked();
                if ui.small_button(icon::X).on_hover_text("Forget").clicked() {
                    forget = Some(i);
                }
            });
        }
        if let Some(i) = forget {
            let Custom { command, .. } = prefs.recent.remove(i);
            if self.command == command && self.picked == Some(Picked::Custom) {
                self.picked = None;
            }
        }
        activated
    }
}
