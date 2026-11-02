//! The mobile layout: one tab at a time filling the window, a bar along the
//! bottom for places, new tabs, the tab switcher and the menu, and the panes
//! as sheets that take the tab's place. It can be switched on on the desktop
//! too, where it shrinks the window to a phone's size.

use egui::{
    Align, Align2, Button, CentralPanel, FontId, Id, Key, Layout, Modifiers, Panel, Rect, RichText,
    Sense, Stroke, Ui, Vec2, vec2,
};
use egui_dock::{DockState, TabViewer};
use egui_phosphor::regular as icon;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

use super::{FilesApp, Viewer};
use crate::{
    dock_tab::{DockTab, Pane},
    touch::TOUCH_SIZE,
};

/// The window's size when switching to the mobile layout on the desktop.
pub const PHONE_SIZE: Vec2 = vec2(400.0, 820.0);
/// The smallest window in the mobile layout.
pub const MIN_SIZE: Vec2 = vec2(320.0, 480.0);
/// The smallest window in the desktop layout.
pub const DESKTOP_MIN_SIZE: Vec2 = vec2(480.0, 320.0);
/// The window's size when switching back without one to return to.
const DESKTOP_SIZE: Vec2 = vec2(1100.0, 700.0);

/// How tall the bottom bar is.
const BAR_HEIGHT: f32 = 56.0;
/// Tab cards in the switcher.
const CARD_MIN_WIDTH: f32 = 160.0;
const CARD_HEIGHT: f32 = 92.0;

/// Whether the app runs on a phone, which only has the mobile layout.
pub const ONLY_MOBILE: bool = cfg!(target_os = "android");

/// What takes the tab's place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sheet {
    Tabs,
    Menu,
    Pane(Pane),
}

#[derive(Default)]
pub struct Mobile {
    pub enabled: bool,
    pub sheet: Option<Sheet>,
    /// The window's size before switching to the mobile layout, to go back to.
    desktop_size: Option<Vec2>,
    /// The tab each tab was opened or shown from, which going back from it
    /// returns to.
    openers: HashMap<u64, u64>,
    /// The tab shown last frame: what a tab shown since was shown from.
    last: Option<u64>,
    /// The tab shown changed in a way that isn't going forward to it: from
    /// the tab switcher, or going back.
    no_opener: bool,
}

/// What the user asked the layout for, done by the app.
#[derive(Debug, PartialEq)]
pub enum Request {
    NewTab,
    Focus(u64),
    Close(u64),
    OpenTrash,
    DiskUsage,
    Properties,
    Settings,
    About,
    Desktop,
    Quit,
}

impl Mobile {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: enabled || ONLY_MOBILE,
            ..Self::default()
        }
    }

    /// Opens `sheet`, or closes it if it's open.
    pub fn toggle(&mut self, sheet: Sheet) {
        self.sheet = if self.sheet == Some(sheet) {
            None
        } else {
            Some(sheet)
        };
    }

    /// Resizes the window for the layout being switched to.
    pub fn resize_window(&mut self, ctx: &egui::Context) {
        if ONLY_MOBILE {
            return;
        }
        use egui::ViewportCommand as C;
        let send = |command| ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, command);
        if self.enabled {
            self.desktop_size = ctx.input(|i| i.viewport().inner_rect).map(|r| r.size());
            send(C::Maximized(false));
            send(C::MinInnerSize(MIN_SIZE));
            send(C::InnerSize(PHONE_SIZE));
        } else {
            send(C::MinInnerSize(DESKTOP_MIN_SIZE));
            send(C::InnerSize(
                self.desktop_size.take().unwrap_or(DESKTOP_SIZE),
            ));
        }
    }
}

impl FilesApp {
    /// Switches between the mobile and desktop layouts.
    pub(super) fn set_mobile(&mut self, ctx: &egui::Context, on: bool) {
        if self.mobile.enabled == on {
            return;
        }
        self.mobile.enabled = on;
        self.mobile.sheet = None;
        if on {
            // One window: tab windows' tabs join the main one's.
            for window in std::mem::take(&mut self.windows) {
                let mut dock = window.dock;
                let ids: Vec<u64> = dock.iter_all_tabs().map(|(_, t)| t.id()).collect();
                for id in ids {
                    if let Some(path) = dock.find_tab_from(|t| t.id() == id)
                        && let Some(tab) = dock.remove_tab(path)
                    {
                        self.push_main(tab);
                    }
                }
            }
            self.focused_window = None;
        }
        self.mobile.resize_window(ctx);
    }

    /// Notes which tab each tab was opened or shown from, as a browser's
    /// back does; call every frame with the tab shown.
    pub(super) fn track_openers(&mut self, current: Option<u64>) {
        let open: HashSet<u64> = self
            .dock
            .iter_all_tabs()
            .filter(|(_, t)| t.pane().is_none())
            .map(|(_, t)| t.id())
            .collect();
        let mobile = &mut self.mobile;
        // Set during last frame, when any switch it caused happened.
        let no_opener = std::mem::take(&mut mobile.no_opener);
        if let (Some(current), Some(last)) = (current, mobile.last)
            && current != last
            && !no_opener
        {
            mobile.openers.insert(current, last);
        }
        mobile
            .openers
            .retain(|id, opener| open.contains(id) && open.contains(opener));
        mobile.last = current;
    }

    /// Android's back button or gesture: closes a menu or the sheet, goes back in the
    /// tab's history, or back to the tab it was opened from (closing it),
    /// and with nowhere left to go, leaves the app as Android apps do.
    pub(super) fn mobile_back(&mut self, ctx: &egui::Context, current: Option<u64>) {
        if egui::Popup::is_any_open(ctx) {
            egui::Popup::close_all(ctx);
            return;
        }
        if let Some(sheet) = self.mobile.sheet.take() {
            if sheet == Sheet::Pane(Pane::Terminal) {
                self.terminal.surrender_focus(ctx);
            }
            return;
        }
        let Some(id) = current else {
            return;
        };
        let tab = self
            .dock
            .iter_all_tabs_mut()
            .find(|(_, t)| t.id() == id)
            .map(|(_, t)| t);
        let is_browser = match tab {
            Some(DockTab::Browser(browser)) => {
                if browser.selecting {
                    browser.queued.push(crate::tab::Action::ClearSelection);
                    return;
                }
                if browser.can_go_back() {
                    browser.queued.push(crate::tab::Action::Back);
                    return;
                }
                true
            }
            Some(_) => false,
            None => return,
        };
        // An editor with unsaved changes asks first, and stays if cancelled.
        if self.editor_mut(id).is_some_and(|e| e.is_dirty()) {
            self.request_close(id);
            return;
        }
        let opener = self
            .mobile
            .openers
            .get(&id)
            .copied()
            .filter(|&opener| self.find_tab(|t| t.id() == opener).is_some());
        // The last browser tab and pinned tabs stay open.
        let closeable = !self.pinned.contains(&id) && (!is_browser || self.browser_count() > 1);
        self.mobile.no_opener = true;
        if closeable && (opener.is_some() || !is_browser) {
            self.request_close(id);
            if let Some(opener) = opener {
                self.focus_tab(opener);
            }
        }
    }

    /// The tab the mobile layout shows: the focused one, or the browser tab
    /// the panes follow if a pane has focus. Gives it the focus.
    pub(super) fn mobile_current(&mut self) -> Option<u64> {
        let focused = self
            .dock
            .find_active_focused()
            .filter(|(_, t)| t.pane().is_none())
            .map(|(_, t)| t.id());
        if focused.is_some() {
            return focused;
        }
        let id = self.active_browser.or_else(|| {
            self.dock
                .iter_all_tabs()
                .find(|(_, t)| t.pane().is_none())
                .map(|(_, t)| t.id())
        })?;
        self.focus_tab(id);
        Some(id)
    }

    pub(super) fn mobile_request(&mut self, ctx: &egui::Context, request: Request) {
        match request {
            Request::NewTab => {
                let path = self.focused_path();
                self.open_tab(None, path);
                self.mobile.sheet = None;
            }
            Request::Focus(id) => {
                self.focus_tab(id);
                self.mobile.no_opener = true;
            }
            Request::Close(id) => {
                if !self.pinned.contains(&id) {
                    self.request_close(id);
                }
            }
            Request::OpenTrash => self.open_trash(),
            Request::DiskUsage => {
                let path = self.focused_path();
                self.open_usage(None, path);
            }
            Request::Properties => {
                if let Some(source) = self.focused_selection() {
                    self.open_properties(source);
                }
            }
            Request::Settings => self.open_settings(None),
            Request::About => self.about.open = true,
            Request::Desktop => self.set_mobile(ctx, false),
            Request::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
        // Whatever was asked for shows instead of the sheet.
        if !matches!(request, Request::Close(_)) {
            self.mobile.sheet = None;
        }
    }
}

/// Whether the mobile layout is on in eframe's saved state, to open the
/// window at the right size before the app starts.
pub fn saved_enabled(app_id: &str) -> bool {
    eframe::storage_dir(app_id)
        .and_then(|dir| std::fs::read_to_string(dir.join("app.ron")).ok())
        .is_some_and(|text| enabled_in(&text))
}

/// Whether the mobile layout is on in `app_ron`: a map of keys to the RON
/// of each value, the app's under [`eframe::APP_KEY`].
fn enabled_in(app_ron: &str) -> bool {
    #[derive(Deserialize)]
    struct Saved {
        #[serde(default)]
        mobile: bool,
    }
    ron::from_str::<std::collections::HashMap<String, String>>(app_ron)
        .ok()
        .and_then(|kv| ron::from_str::<Saved>(kv.get(eframe::APP_KEY)?).ok())
        .is_some_and(|saved| saved.mobile)
}

/// Draws the layout: the bottom bar, then the tab `current` or the open sheet.
pub fn show(
    ui: &mut Ui,
    dock: &mut DockState<DockTab>,
    mobile: &mut Mobile,
    viewer: &mut Viewer,
    current: Option<u64>,
) -> Vec<Request> {
    let mut requests = Vec::new();
    // Escape closes a sheet, except the terminal's, which the shell needs.
    if mobile
        .sheet
        .is_some_and(|sheet| sheet != Sheet::Pane(Pane::Terminal))
        && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
    {
        mobile.sheet = None;
    }
    let tabs = dock
        .iter_all_tabs()
        .filter(|(_, t)| t.pane().is_none())
        .count();
    Panel::bottom("mobile_bar")
        .exact_size(BAR_HEIGHT)
        .resizable(false)
        .show(ui, |ui| bar(ui, mobile, tabs, &mut requests));
    if !viewer.shared.jobs.is_empty() {
        Panel::bottom("jobs").show(ui, |ui| viewer.shared.jobs.ui(ui));
    }
    CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show(ui, |ui| match mobile.sheet {
            None => {
                let tab = current.and_then(|id| {
                    dock.iter_all_tabs_mut()
                        .find(|(_, t)| t.id() == id)
                        .map(|(_, t)| t)
                });
                match tab {
                    Some(tab) => {
                        ui.push_id(("mobile_tab", tab.id()), |ui| viewer.ui(ui, tab))
                            .inner
                    }
                    None => {
                        ui.centered_and_justified(|ui| ui.weak("No tabs"));
                    }
                }
            }
            Some(Sheet::Pane(pane)) => {
                if header(ui, &format!("{} {}", pane.icon(), pane.label()), |_| {}) {
                    mobile.sheet = None;
                }
                viewer.pane_ui(ui, pane);
            }
            Some(Sheet::Tabs) => {
                if switcher(ui, dock, viewer, current, tabs, &mut requests) {
                    mobile.sheet = None;
                }
            }
            Some(Sheet::Menu) => {
                if header(ui, "Menu", |_| {}) {
                    mobile.sheet = None;
                }
                menu(ui, mobile, viewer, &mut requests);
            }
        });
    requests
}

/// The bottom bar: places, a new tab, the tab switcher and the menu.
fn bar(ui: &mut Ui, mobile: &mut Mobile, tabs: usize, requests: &mut Vec<Request>) {
    let places = Sheet::Pane(Pane::Places);
    let width = ui.available_width() / 4.0;
    let size = vec2(width, ui.available_height());
    ui.spacing_mut().item_spacing.x = 0.0;
    ui.horizontal_centered(|ui| {
        let button = |ui: &mut Ui, text: &str, on: bool| {
            ui.add_sized(
                size,
                Button::selectable(on, RichText::new(text).size(22.0)).frame_when_inactive(false),
            )
        };
        if button(ui, icon::COMPASS, mobile.sheet == Some(places))
            .on_hover_text("Places")
            .clicked()
        {
            mobile.toggle(places);
        }
        if button(ui, icon::PLUS, false)
            .on_hover_text("New tab")
            .clicked()
        {
            requests.push(Request::NewTab);
            mobile.sheet = None;
        }
        let on = mobile.sheet == Some(Sheet::Tabs);
        let response = button(ui, "", on).on_hover_text("Tabs");
        tab_count(ui, response.rect.center(), tabs);
        if response.clicked() {
            mobile.toggle(Sheet::Tabs);
        }
        if button(
            ui,
            icon::DOTS_THREE_VERTICAL,
            mobile.sheet == Some(Sheet::Menu),
        )
        .on_hover_text("Menu")
        .clicked()
        {
            mobile.toggle(Sheet::Menu);
        }
    });
}

/// The number of tabs in a rounded square, as mobile browsers show it.
fn tab_count(ui: &Ui, center: egui::Pos2, tabs: usize) {
    let color = ui.visuals().text_color();
    let rect = Rect::from_center_size(center, Vec2::splat(22.0));
    ui.painter()
        .rect_stroke(rect, 5.0, Stroke::new(1.8, color), egui::StrokeKind::Inside);
    let (text, size) = if tabs > 99 {
        ("99+".to_owned(), 8.0)
    } else {
        (tabs.to_string(), 12.0)
    };
    ui.painter().text(
        center,
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(size),
        color,
    );
}

/// A sheet's title with a button back to the tab, and room on the right for
/// `extra`. Returns whether the back button was pressed.
fn header(ui: &mut Ui, title: &str, extra: impl FnOnce(&mut Ui)) -> bool {
    let mut back = false;
    Panel::top(Id::new(("mobile_header", title)))
        .exact_size(BAR_HEIGHT)
        .resizable(false)
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                back = ui
                    .add_sized(
                        Vec2::splat(TOUCH_SIZE),
                        Button::new(RichText::new(icon::ARROW_LEFT).size(20.0))
                            .frame_when_inactive(false),
                    )
                    .on_hover_text("Back")
                    .clicked();
                ui.label(RichText::new(title).heading());
                ui.with_layout(Layout::right_to_left(Align::Center), extra);
            });
        });
    back
}

/// The tab switcher: a card for each tab, tapped to switch to it. Returns
/// whether it's done.
fn switcher(
    ui: &mut Ui,
    dock: &mut DockState<DockTab>,
    viewer: &mut Viewer,
    current: Option<u64>,
    count: usize,
    requests: &mut Vec<Request>,
) -> bool {
    let title = if count == 1 {
        "1 tab".to_owned()
    } else {
        format!("{count} tabs")
    };
    let mut done = header(ui, &title, |ui| {
        if ui.button(format!("{} New tab", icon::PLUS)).clicked() {
            requests.push(Request::NewTab);
        }
    });
    let gap = ui.spacing().item_spacing.x;
    egui::ScrollArea::vertical()
        .scroll_source(crate::touch::scroll_source(ui.ctx()))
        .auto_shrink(false)
        .show(ui, |ui| {
            egui::Frame::NONE.inner_margin(gap).show(ui, |ui| {
                let width = ui.available_width();
                let columns = (((width + gap) / (CARD_MIN_WIDTH + gap)).floor() as usize).max(1);
                let card_width = (width - gap * (columns - 1) as f32) / columns as f32;
                let mut tabs: Vec<_> = dock
                    .iter_all_tabs_mut()
                    .filter(|(_, t)| t.pane().is_none())
                    .collect();
                for row in tabs.chunks_mut(columns) {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        for (path, tab) in row {
                            let size = vec2(card_width, CARD_HEIGHT);
                            if card(ui, size, viewer, tab, *path, current, requests) {
                                done = true;
                            }
                        }
                    });
                }
            });
        });
    done
}

/// One tab's card. Returns whether it was tapped.
fn card(
    ui: &mut Ui,
    size: Vec2,
    viewer: &mut Viewer,
    tab: &mut DockTab,
    path: egui_dock::TabPath,
    current: Option<u64>,
    requests: &mut Vec<Request>,
) -> bool {
    let id = tab.id();
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let visuals = ui.visuals();
    let selected = current == Some(id);
    let stroke = if selected {
        Stroke::new(2.0, visuals.selection.stroke.color)
    } else {
        visuals.widgets.noninteractive.bg_stroke
    };
    let fill = if response.hovered() {
        visuals.widgets.hovered.weak_bg_fill
    } else {
        visuals.faint_bg_color
    };
    let radius = visuals.window_corner_radius;
    ui.painter()
        .rect(rect, radius, fill, stroke, egui::StrokeKind::Inside);

    let inner = rect.shrink(10.0);
    let close = viewer.is_closeable(tab).then(|| {
        Rect::from_min_size(
            egui::pos2(rect.right() - TOUCH_SIZE, rect.top()),
            Vec2::splat(TOUCH_SIZE),
        )
    });
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner.with_max_x(close.map_or(inner.right(), |r| r.left())))
            .layout(Layout::top_down(Align::LEFT)),
    );
    child.add(
        egui::Label::new(RichText::new(viewer.title(tab).text()).strong())
            .truncate()
            .selectable(false),
    );
    if let Some(subtitle) = subtitle(tab) {
        child.add(
            egui::Label::new(RichText::new(subtitle).weak().small())
                .wrap()
                .selectable(false),
        );
    }
    if let Some(close) = close {
        let response = ui
            .put(
                close,
                Button::new(RichText::new(icon::X).size(16.0)).frame_when_inactive(false),
            )
            .on_hover_text("Close tab");
        if response.clicked() {
            requests.push(Request::Close(id));
            return false;
        }
    }
    response.context_menu(|ui| viewer.context_menu(ui, tab, path.node_path()));
    if response.clicked() {
        requests.push(Request::Focus(id));
        return true;
    }
    false
}

/// What a tab's card says under its title.
fn subtitle(tab: &DockTab) -> Option<String> {
    match tab {
        DockTab::Browser(browser) => Some(browser.path.display().to_string()),
        DockTab::Editor(editor) => Some(editor.edit.display().to_string()),
        DockTab::Trash(_) => Some("Trash".to_owned()),
        DockTab::Archive(_) => Some("Archive".to_owned()),
        DockTab::Usage(_) => Some("Disk usage".to_owned()),
        DockTab::Properties(_) => Some("Properties".to_owned()),
        DockTab::Settings(_) | DockTab::Pane(_) => None,
    }
}

/// The menu sheet: the panes, the File menu's commands and quick settings.
fn menu(ui: &mut Ui, mobile: &mut Mobile, viewer: &mut Viewer, requests: &mut Vec<Request>) {
    egui::ScrollArea::vertical()
        .scroll_source(crate::touch::scroll_source(ui.ctx()))
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.spacing_mut().button_padding.x = 16.0;
            let width = ui.available_width();
            let item = |ui: &mut Ui, icon: &str, label: &str| {
                ui.add_sized(
                    vec2(width, TOUCH_SIZE),
                    Button::new(format!("{icon}  {label}"))
                        .frame_when_inactive(false)
                        .right_text(""),
                )
                .clicked()
            };
            for pane in [Pane::Preview, Pane::Tree, Pane::Terminal] {
                if item(ui, pane.icon(), pane.label()) {
                    mobile.sheet = Some(Sheet::Pane(pane));
                }
            }
            ui.separator();
            let commands = [
                (icon::TRASH, "Trash", Request::OpenTrash),
                (icon::CHART_PIE_SLICE, "Disk usage", Request::DiskUsage),
                (icon::INFO, "Properties", Request::Properties),
                (icon::GEAR, "Settings", Request::Settings),
            ];
            for (icon, label, request) in commands {
                if item(ui, icon, label) {
                    requests.push(request);
                }
            }
            ui.separator();
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(16, 0))
                .show(ui, |ui| {
                    ui.checkbox(&mut viewer.shared.show_hidden, "Show hidden files");
                    ui.checkbox(&mut viewer.shared.show_thumbnails, "Show thumbnails");
                });
            ui.separator();
            let commands = [
                (icon::DESKTOP, "Desktop layout", Request::Desktop),
                (icon::QUESTION, "About egui-files", Request::About),
                (icon::SIGN_OUT, "Quit", Request::Quit),
            ];
            for (icon, label, request) in commands {
                // Phones only have the mobile layout.
                if request == Request::Desktop && ONLY_MOBILE {
                    continue;
                }
                if item(ui, icon, label) {
                    requests.push(request);
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_state_says_whether_it_is_on() {
        let saved = |app: &str| {
            let mut kv = std::collections::HashMap::new();
            kv.insert(eframe::APP_KEY.to_owned(), app.to_owned());
            ron::to_string(&kv).unwrap()
        };
        assert!(enabled_in(&saved("(mobile: true, show_hidden: false)")));
        assert!(!enabled_in(&saved("(mobile: false)")));
        assert!(!enabled_in(&saved("(show_hidden: false)")));
        assert!(!enabled_in("garbage"));
    }

    #[test]
    fn sheets_toggle() {
        let mut mobile = Mobile::new(true);
        mobile.toggle(Sheet::Tabs);
        assert_eq!(mobile.sheet, Some(Sheet::Tabs));
        mobile.toggle(Sheet::Menu);
        assert_eq!(mobile.sheet, Some(Sheet::Menu));
        mobile.toggle(Sheet::Menu);
        assert_eq!(mobile.sheet, None);
    }
}
