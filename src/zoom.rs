//! The whole app's zoom, which egui changes on Ctrl and +, - or 0 and saves
//! across runs. Everything grows with it, so a zoom nobody meant to make looks
//! like something's wrong; while it's off 100% the menu bar says so, as a web
//! browser's address bar does, with the way back one click away.

use egui::{
    Button, Context, PopupCloseBehavior, Ui,
    containers::menu::{MenuButton, MenuConfig},
    gui_zoom,
};
use egui_phosphor::regular as icon;

const DEFAULT_PERCENT: i32 = 100;

/// The zoom in whole percent, which is what can be told apart on screen.
fn percent(ctx: &Context) -> i32 {
    (ctx.zoom_factor() * 100.0).round() as i32
}

pub fn is_default(ctx: &Context) -> bool {
    percent(ctx) == DEFAULT_PERCENT
}

/// The zoom as a label, like "110%".
pub fn label(ctx: &Context) -> String {
    format!("{}%", percent(ctx))
}

/// Smaller, the zoom, larger and back to 100%.
pub fn controls(ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    ui.horizontal(|ui| {
        if ui
            .button(icon::MINUS)
            .on_hover_text(hint(&ctx, "Zoom out", &gui_zoom::kb_shortcuts::ZOOM_OUT))
            .clicked()
        {
            gui_zoom::zoom_out(&ctx);
        }
        ui.label(label(&ctx));
        if ui
            .button(icon::PLUS)
            .on_hover_text(hint(&ctx, "Zoom in", &gui_zoom::kb_shortcuts::ZOOM_IN))
            .clicked()
        {
            gui_zoom::zoom_in(&ctx);
        }
        let reset = ui
            .add_enabled(!is_default(&ctx), Button::new("Reset"))
            .on_hover_text(hint(
                &ctx,
                "Back to 100%",
                &gui_zoom::kb_shortcuts::ZOOM_RESET,
            ));
        if reset.clicked() {
            ctx.set_zoom_factor(1.0);
        }
    });
}

fn hint(ctx: &Context, label: &str, shortcut: &egui::KeyboardShortcut) -> String {
    format!("{label} ({})", ctx.format_shortcut(shortcut))
}

/// A button for the menu bar saying how far the app is zoomed, opening the
/// [`controls`]. Shows nothing at 100%.
pub fn indicator(ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    if is_default(&ctx) {
        return;
    }
    let glass = if percent(&ctx) > DEFAULT_PERCENT {
        icon::MAGNIFYING_GLASS_PLUS
    } else {
        icon::MAGNIFYING_GLASS_MINUS
    };
    // Open through several clicks of + and -.
    let config = MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside);
    let (button, _) = MenuButton::new(format!("{glass} {}", label(&ctx)))
        .config(config)
        .ui(ui, controls);
    button.on_hover_text("The whole app is zoomed. Click to change it or go back to 100%");
}
