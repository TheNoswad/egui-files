//! "Sign in to Proton Drive": email and password, then what's also wanted: a
//! CAPTCHA solved in the browser if Proton asks for one, the authenticator
//! app's code, and a second password if the account has one for its data.

use std::sync::{
    Mutex, MutexGuard, PoisonError,
    mpsc::{self, Receiver},
};

use egui::{Context, Id, Key, RichText, TextEdit};
use egui_phosphor::regular as icon;
use proton_sdk::{api::HumanVerification, session::ProtonApiSession};
use zeroize::Zeroizing;

use super::Step;

#[derive(Default)]
struct Dialog {
    email: String,
    password: Zeroizing<String>,
    code: String,
    mailbox: Zeroizing<String>,
    /// Past the first step: the session that's being signed in, and what's
    /// asked of it next.
    session: Option<(ProtonApiSession, Wanted)>,
    /// The CAPTCHA Proton wants solved before signing in can go on.
    challenge: Option<HumanVerification>,
    error: Option<String>,
    task: Option<Receiver<Result<Step, String>>>,
    focus: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wanted {
    Code,
    Mailbox,
}

static DIALOG: Mutex<Option<Dialog>> = Mutex::new(None);

fn dialog() -> MutexGuard<'static, Option<Dialog>> {
    DIALOG.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn is_open() -> bool {
    dialog().is_some()
}

/// Opens the dialog, unless it's open already.
pub fn open() {
    let mut dialog = dialog();
    if dialog.is_none() {
        *dialog = Some(Dialog {
            focus: true,
            ..Dialog::default()
        });
        drop(dialog);
        crate::remote::repaint();
    }
}

fn start(dialog: &mut Dialog, work: impl FnOnce() -> Result<Step, String> + Send + 'static) {
    let (send, receive) = mpsc::channel();
    crate::background::spawn("sign in to Proton Drive", move || {
        let _ = send.send(work());
        crate::remote::repaint();
    });
    dialog.task = Some(receive);
    dialog.error = None;
}

/// Sends what the current step wanted.
fn submit(dialog: &mut Dialog) {
    let password = dialog.password.clone();
    match dialog.session.clone() {
        None => {
            let email = dialog.email.trim().to_owned();
            let solved = dialog.challenge.clone();
            start(dialog, move || {
                super::begin(&email, &password, solved.as_ref())
            });
        }
        Some((session, Wanted::Code)) => {
            let code = dialog.code.trim().to_owned();
            start(dialog, move || {
                super::submit_code(session, &code, &password)
            });
        }
        Some((session, Wanted::Mailbox)) => {
            let mailbox = dialog.mailbox.clone();
            start(dialog, move || super::finish(session, &mailbox));
        }
    }
}

fn can_submit(dialog: &Dialog) -> bool {
    dialog.task.is_none()
        && match &dialog.session {
            None if dialog.challenge.is_some() => true,
            None => !dialog.email.trim().is_empty() && !dialog.password.is_empty(),
            Some((_, Wanted::Code)) => !dialog.code.trim().is_empty(),
            Some((_, Wanted::Mailbox)) => !dialog.mailbox.is_empty(),
        }
}

/// Shows the dialog if it's open.
pub fn ui(ctx: &Context) {
    let mut guard = dialog();
    let Some(dialog) = guard.as_mut() else { return };

    if let Some(Ok(result)) = dialog.task.as_ref().map(Receiver::try_recv) {
        dialog.task = None;
        match result {
            Ok(Step::Done) => {
                *guard = None;
                return;
            }
            Ok(Step::Verify(challenge)) => {
                if dialog.challenge.is_some() {
                    dialog.error = Some(
                        "Proton doesn't see the CAPTCHA as solved yet. Open it again, solve it, \
                         then press Continue."
                            .to_owned(),
                    );
                }
                dialog.challenge = Some(challenge);
            }
            Ok(Step::Code(session)) => {
                dialog.challenge = None;
                dialog.session = Some((session, Wanted::Code));
                dialog.focus = true;
            }
            Ok(Step::Mailbox(session)) => {
                dialog.challenge = None;
                dialog.session = Some((session, Wanted::Mailbox));
                dialog.focus = true;
            }
            Err(why) => {
                dialog.error = Some(why);
                // A code is only good once.
                dialog.code.clear();
                dialog.focus = true;
            }
        }
    }

    let busy = dialog.task.is_some();
    let wanted = dialog.session.as_ref().map(|(_, wanted)| *wanted);
    let captcha_url = dialog.challenge.as_ref().map(super::browser_url);
    let mut entered = false;
    let mut close = false;
    let modal = egui::Modal::new(Id::new("proton_sign_in")).show(ctx, |ui| {
        ui.set_max_width(crate::widgets::modal_width(ui, 420.0));
        ui.heading(format!("{} Sign in to Proton Drive", icon::CLOUD));
        ui.label(
            RichText::new(
                "This app isn't made by Proton. Your password goes only to Proton and isn't \
                 kept; this computer's keyring keeps you signed in.",
            )
            .weak(),
        );
        ui.add_space(8.0);
        if let Some(url) = &captcha_url {
            ui.label(
                "Proton wants a CAPTCHA solved for this sign-in. Solve it in your browser, then \
                 come back here and press Continue.",
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui
                    .button(format!("{} Open in browser", icon::GLOBE))
                    .clicked()
                    && let Err(err) = crate::fs::open_detached(url)
                {
                    dialog.error = Some(format!("Couldn't open a browser: {err}"));
                }
                if ui.button(format!("{} Copy link", icon::COPY)).clicked() {
                    ui.ctx().copy_text(url.clone());
                }
            });
        }
        ui.add_enabled_ui(!busy, |ui| {
            if captcha_url.is_some() {
                return;
            }
            let mut field = |ui: &mut egui::Ui, edit: TextEdit<'_>, first: bool, secret: bool| {
                let response = ui.add(edit.desired_width(240.0));
                // Its undo history would keep each version of what's typed.
                if secret && let Some(mut state) = TextEdit::load_state(ui.ctx(), response.id) {
                    state.clear_undoer();
                    TextEdit::store_state(ui.ctx(), response.id, state);
                }
                if first && std::mem::take(&mut dialog.focus) {
                    response.request_focus();
                }
                entered |= response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            };
            egui::Grid::new("proton_fields")
                .num_columns(2)
                .spacing([8.0, 6.0])
                .show(ui, |ui| match wanted {
                    None => {
                        ui.label("Email");
                        field(ui, TextEdit::singleline(&mut dialog.email), true, false);
                        ui.end_row();
                        ui.label("Password");
                        field(
                            ui,
                            TextEdit::singleline(&mut *dialog.password).password(true),
                            false,
                            true,
                        );
                        ui.end_row();
                    }
                    Some(Wanted::Code) => {
                        ui.label("Code");
                        field(
                            ui,
                            TextEdit::singleline(&mut dialog.code)
                                .hint_text("from your authenticator app"),
                            true,
                            false,
                        );
                        ui.end_row();
                    }
                    Some(Wanted::Mailbox) => {
                        ui.label("Data password");
                        field(
                            ui,
                            TextEdit::singleline(&mut *dialog.mailbox).password(true),
                            true,
                            true,
                        );
                        ui.end_row();
                    }
                });
        });
        if let Some(why) = &dialog.error {
            ui.add_space(4.0);
            ui.colored_label(ui.visuals().error_fg_color, why);
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let ready = can_submit(dialog);
            let label = if wanted.is_some() || captcha_url.is_some() {
                "Continue"
            } else {
                "Sign in"
            };
            if ui.add_enabled(ready, egui::Button::new(label)).clicked() || (entered && ready) {
                submit(dialog);
            }
            if ui.add_enabled(!busy, egui::Button::new("Cancel")).clicked() {
                close = true;
            }
            if dialog.task.is_some() {
                ui.spinner();
            }
        });
    });
    if close || (modal.should_close() && !busy) {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_captcha_can_go_on_without_typing_anything() {
        let mut dialog = Dialog::default();
        assert!(!can_submit(&dialog));
        dialog.challenge = Some(HumanVerification {
            token: "t".to_owned(),
            methods: vec!["captcha".to_owned()],
        });
        assert!(can_submit(&dialog));
    }
}
