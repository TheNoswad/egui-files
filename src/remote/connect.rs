//! "Connect to server": a computer to connect to, picked from the ssh
//! config's hosts and others known, or typed in; and optionally who to be
//! there, its port, and which folder to open.

use std::{path::PathBuf, sync::Mutex};

use egui::{Context, Id, Key, RichText, TextEdit};
use egui_phosphor::regular as icon;

#[derive(Default)]
struct Dialog {
    host: String,
    user: String,
    port: String,
    folder: String,
    focus: bool,
}

static DIALOG: Mutex<Option<Dialog>> = Mutex::new(None);

/// Opens the dialog, with `host` filled in if given.
pub fn open(host: Option<&str>) {
    *DIALOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Dialog {
        host: host.unwrap_or_default().to_owned(),
        focus: true,
        ..Dialog::default()
    });
}

/// Where the user asked to go: `user@host:port` as typed, or `None` if it
/// doesn't make one.
fn authority(dialog: &Dialog) -> Option<String> {
    let host = dialog.host.trim();
    if host.is_empty() {
        return None;
    }
    let mut authority = String::new();
    let user = dialog.user.trim();
    if !user.is_empty() && !host.contains('@') {
        authority.push_str(user);
        authority.push('@');
    }
    // An IPv6 address needs brackets before a port.
    let bracketed = host.contains(':') && !host.starts_with('[');
    let port = dialog.port.trim();
    if bracketed && !port.is_empty() {
        authority.push_str(&format!("[{host}]"));
    } else {
        authority.push_str(host);
    }
    if !port.is_empty() {
        authority.push(':');
        authority.push_str(port);
    }
    #[cfg(not(target_arch = "wasm32"))]
    disk::sftp::Target::parse(&authority)?;
    Some(authority)
}

fn target(dialog: &Dialog) -> Option<PathBuf> {
    let authority = authority(dialog)?;
    let folder = dialog.folder.trim();
    Some(if folder.is_empty() {
        super::home_of(&authority)
    } else if let Some(rest) = folder.strip_prefix('~') {
        PathBuf::from(format!("sftp://{authority}/~{rest}"))
    } else {
        PathBuf::from(format!(
            "sftp://{authority}/{}",
            folder.trim_start_matches('/')
        ))
    })
}

/// Shows the dialog if it's open; returns where to go once the user says.
pub fn ui(ctx: &Context) -> Option<PathBuf> {
    let mut guard = DIALOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dialog = guard.as_mut()?;
    let mut go = None;
    let mut close = false;
    let modal = egui::Modal::new(Id::new("connect_to_server")).show(ctx, |ui| {
        ui.set_max_width(crate::widgets::modal_width(ui, 460.0));
        ui.heading(format!("{} Connect to server", icon::HARD_DRIVES));
        ui.label(
            RichText::new("Over SFTP, with ssh: your ssh config, keys and agent apply.").weak(),
        );
        ui.add_space(8.0);
        if let Err(why) = super::available() {
            ui.colored_label(ui.visuals().warn_fg_color, why);
            ui.add_space(8.0);
        }
        let hosts = super::hosts::hosts();
        let mut entered = false;
        egui::Grid::new("connect_fields")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Server");
                ui.horizontal(|ui| {
                    let field = ui.add(
                        TextEdit::singleline(&mut dialog.host)
                            .hint_text("host, alias or user@host")
                            .desired_width(240.0),
                    );
                    if std::mem::take(&mut dialog.focus) {
                        field.request_focus();
                    }
                    entered |= field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    let suggestions = hosts.matching("", &super::recent());
                    ui.add_enabled_ui(!suggestions.is_empty(), |ui| {
                        ui.menu_button(icon::CARET_DOWN, |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(300.0)
                                .show(ui, |ui| {
                                    for name in suggestions {
                                        let detail = hosts
                                            .config
                                            .iter()
                                            .find(|h| h.alias == name)
                                            .and_then(|h| h.detail.clone());
                                        let button = egui::Button::new(&name).right_text(
                                            RichText::new(detail.unwrap_or_default()).weak(),
                                        );
                                        if ui.add(button).clicked() {
                                            dialog.host = name;
                                            ui.close();
                                        }
                                    }
                                });
                        })
                        .response
                        .on_hover_text("Hosts from your ssh config, and others you've used");
                    });
                });
                ui.end_row();
                ui.label("User");
                let field = ui.add(
                    TextEdit::singleline(&mut dialog.user)
                        .hint_text("as ssh config says, or yours")
                        .desired_width(240.0),
                );
                entered |= field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.end_row();
                ui.label("Port");
                let field = ui.add(
                    TextEdit::singleline(&mut dialog.port)
                        .hint_text("22")
                        .desired_width(80.0),
                );
                entered |= field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.end_row();
                ui.label("Folder");
                let field = ui.add(
                    TextEdit::singleline(&mut dialog.folder)
                        .hint_text("your home folder")
                        .desired_width(240.0),
                );
                entered |= field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.end_row();
            });
        let port_ok = dialog.port.trim().is_empty() || dialog.port.trim().parse::<u16>().is_ok();
        let target = target(dialog).filter(|_| port_ok && super::available().is_ok());
        if !dialog.host.trim().is_empty() && target.is_none() && super::available().is_ok() {
            ui.colored_label(
                ui.visuals().error_fg_color,
                "That isn't a server ssh can connect to.",
            );
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let connect = ui.add_enabled(target.is_some(), egui::Button::new("Connect"));
            if connect.clicked() || (entered && target.is_some()) {
                go = target.clone();
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if go.is_some() || close || modal.should_close() {
        *guard = None;
    }
    go
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog(host: &str, user: &str, port: &str, folder: &str) -> Dialog {
        Dialog {
            host: host.into(),
            user: user.into(),
            port: port.into(),
            folder: folder.into(),
            focus: false,
        }
    }

    #[test]
    fn fields_make_a_path() {
        let path = |d: Dialog| target(&d).map(|p| p.display().to_string());
        assert_eq!(
            path(dialog("pi", "", "", "")).as_deref(),
            Some("sftp://pi/~")
        );
        assert_eq!(
            path(dialog("nas", "me", "2222", "/srv/media")).as_deref(),
            Some("sftp://me@nas:2222/srv/media")
        );
        assert_eq!(
            path(dialog("::1", "", "22", "~/x")).as_deref(),
            Some("sftp://[::1]:22/~/x")
        );
        // The user's in the host already.
        assert_eq!(
            path(dialog("me@pi", "other", "", "")).as_deref(),
            Some("sftp://me@pi/~")
        );
        assert_eq!(path(dialog("", "", "", "")), None);
        #[cfg(not(target_arch = "wasm32"))]
        assert_eq!(path(dialog("-oProxyCommand=x", "", "", "")), None);
    }
}
