//! What ssh asks while connecting (passwords, passphrases, new host keys),
//! put to the user one at a time. Answers are kept only until sent.

use std::{
    collections::VecDeque,
    sync::{
        Mutex, MutexGuard, PoisonError,
        mpsc::{self, Sender},
    },
};

use disk::sftp::{Prompt, PromptKind};
use egui::{Context, Id, Key, RichText, TextEdit};
use egui_phosphor::regular as icon;

struct Waiting {
    prompt: Prompt,
    reply: Sender<Option<String>>,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<Waiting>,
    /// Notes shown while ssh waits (e.g. "touch your security key"), by host.
    notices: Vec<Prompt>,
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue {
    waiting: VecDeque::new(),
    notices: Vec::new(),
});

fn queue() -> MutexGuard<'static, Queue> {
    QUEUE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What's typed into the front question, kept between frames.
static TYPED: Mutex<String> = Mutex::new(String::new());

pub fn init(ctx: &egui::Context) {
    let ctx = ctx.clone();
    disk::sftp::set_prompter(move |prompt| {
        if prompt.kind == PromptKind::Notice {
            // ssh doesn't wait for these: it takes them down when done.
            queue().notices.push(prompt);
            ctx.request_repaint();
            return None;
        }
        let (reply, answer) = mpsc::channel();
        queue().waiting.push_back(Waiting { prompt, reply });
        ctx.request_repaint();
        // None if the app quit, or dropped the question.
        answer.recv().ok().flatten()
    });
}

fn answer(reply: Option<String>) {
    if let Some(waiting) = queue().waiting.pop_front() {
        let _ = waiting.reply.send(reply);
    }
    // Not kept: it may be a password.
    TYPED.lock().unwrap_or_else(PoisonError::into_inner).clear();
}

pub fn ui(ctx: &Context) {
    // A notice lasts while its connection is being made.
    queue().notices.retain(|notice| {
        let root = super::home_of(&notice.host);
        super::connecting(&root)
    });
    let notice = queue().notices.first().cloned();
    let front = queue().waiting.front().map(|w| w.prompt.clone());
    let Some(prompt) = front else {
        if let Some(notice) = notice {
            notice_ui(ctx, &notice);
        }
        return;
    };

    let mut reply: Option<Option<String>> = None;
    let modal = egui::Modal::new(Id::new("ssh_prompt")).show(ctx, |ui| {
        ui.set_max_width(crate::widgets::modal_width(ui, 520.0));
        let (heading, ok) = match prompt.kind {
            PromptKind::HostKey => (
                format!("{} Trust {}?", icon::SHIELD_WARNING, prompt.host),
                "Trust and connect",
            ),
            PromptKind::Confirm => (format!("{} {}", icon::KEY, prompt.host), "Allow"),
            _ => (format!("{} Connecting to {}", icon::LOCK, prompt.host), "Connect"),
        };
        ui.heading(heading);
        ui.add_space(4.0);
        if prompt.kind == PromptKind::HostKey {
            ui.label(
                "This computer hasn't been connected to before, so there's no telling yet \
                 that it's the one you mean. If you can, check the fingerprint against \
                 the one its owner gives.",
            );
            ui.add_space(4.0);
            ui.label(RichText::new(&prompt.text).monospace().small());
        } else {
            ui.label(&prompt.text);
        }
        ui.add_space(8.0);
        let mut typed = TYPED.lock().unwrap_or_else(PoisonError::into_inner);
        let mut entered = false;
        if prompt.kind == PromptKind::Secret {
            let field = ui.add(
                TextEdit::singleline(&mut *typed)
                    .password(true)
                    .desired_width(f32::INFINITY),
            );
            if !field.has_focus() && !field.lost_focus() {
                field.request_focus();
            }
            entered = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            ui.add_space(8.0);
        }
        ui.horizontal(|ui| {
            if ui.button(ok).clicked() || entered {
                reply = Some(Some(match prompt.kind {
                    PromptKind::HostKey => "yes".to_owned(),
                    PromptKind::Confirm => "yes".to_owned(),
                    _ => std::mem::take(&mut *typed),
                }));
            }
            if ui.button("Cancel").clicked() {
                reply = Some(match prompt.kind {
                    PromptKind::HostKey => Some("no".to_owned()),
                    _ => None,
                });
            }
        });
    });
    if modal.should_close() && reply.is_none() {
        reply = Some(match prompt.kind {
            PromptKind::HostKey => Some("no".to_owned()),
            _ => None,
        });
    }
    if let Some(reply) = reply {
        answer(reply);
        ctx.request_repaint();
    }
}

fn notice_ui(ctx: &Context, notice: &Prompt) {
    egui::Modal::new(Id::new("ssh_notice")).show(ctx, |ui| {
        ui.set_max_width(crate::widgets::modal_width(ui, 520.0));
        ui.heading(format!("{} Connecting to {}", icon::KEY, notice.host));
        ui.add_space(4.0);
        ui.label(&notice.text);
    });
    ctx.request_repaint_after(std::time::Duration::from_millis(250));
}
