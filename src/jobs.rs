//! Long-running file operations (copy, move, trash, archives) on background
//! threads, with progress, cancellation, questions for the user, and a status
//! bar UI.

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use egui::{Button, Id, Key, Modal, ProgressBar, RichText, TextEdit, Ui};
use egui_phosphor::regular as icon;

use crate::{
    archive::{self, CompressJob, ExtractJob},
    fs::{display_name, format_time, human_size, unique_path},
};

const BUFFER_SIZE: usize = 1 << 20;
const MAX_ERRORS: usize = 50;
/// How long successful jobs stay in the bar after finishing.
const LINGER: Duration = Duration::from_secs(4);

pub enum JobKind {
    Copy {
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
    Move {
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
    Trash {
        paths: Vec<PathBuf>,
    },
    Restore {
        items: Vec<trash::TrashItem>,
    },
    /// Permanently deletes items from the trash.
    Purge {
        items: Vec<trash::TrashItem>,
    },
    Compress(CompressJob),
    Extract(ExtractJob),
    Eject {
        drive: PathBuf,
        label: String,
    },
}

impl JobKind {
    fn title(&self) -> String {
        let items = |n: usize| {
            if n == 1 {
                "1 item".to_owned()
            } else {
                format!("{n} items")
            }
        };
        match self {
            Self::Copy { sources, dest } => {
                format!("Copying {} to {}", items(sources.len()), display_name(dest))
            }
            Self::Move { sources, dest } => {
                format!("Moving {} to {}", items(sources.len()), display_name(dest))
            }
            Self::Trash { paths } => format!("Moving {} to trash", items(paths.len())),
            Self::Restore { items: trashed } => format!("Restoring {}", items(trashed.len())),
            Self::Purge { items: trashed } => {
                format!("Permanently deleting {}", items(trashed.len()))
            }
            Self::Compress(job) => format!(
                "Compressing {} to {}",
                items(job.sources.len()),
                display_name(&job.dest)
            ),
            Self::Extract(job) => job.title(),
            Self::Eject { label, .. } => format!("Ejecting {label}"),
        }
    }
}

/// Shared between the UI and the worker thread.
#[derive(Default)]
pub struct Progress {
    total_bytes: AtomicU64,
    done_bytes: AtomicU64,
    total_items: AtomicU64,
    done_items: AtomicU64,
    cancel: AtomicBool,
    finished: AtomicBool,
    current: Mutex<String>,
    errors: Mutex<Vec<String>>,
    /// A question the worker is waiting on the user for.
    prompt: Mutex<Option<Prompt>>,
    answer: Mutex<Option<Answer>>,
    answered: Condvar,
    outcome: Mutex<Option<Outcome>>,
}

/// Something only the user can decide, asked from a worker thread.
pub enum Prompt {
    Password {
        archive: PathBuf,
        /// The last password tried was wrong.
        wrong: bool,
    },
    /// `incoming` would be placed where `existing` already is.
    Conflict {
        incoming: PathBuf,
        existing: PathBuf,
    },
}

pub enum Answer {
    Password(String),
    Conflict {
        choice: ConflictChoice,
        /// Use the same choice for the rest of the job.
        all: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictChoice {
    Overwrite,
    Skip,
    KeepBoth,
}

/// Remembers "do this for all conflicts", for folders and files apart.
#[derive(Default)]
pub(crate) struct Resolver {
    files: Option<ConflictChoice>,
    folders: Option<ConflictChoice>,
}

impl Resolver {
    /// What to do with `incoming` where `existing` is; `None` if the user
    /// cancelled.
    pub(crate) fn choose(
        &mut self,
        incoming: &Path,
        existing: &Path,
        p: &Progress,
    ) -> Option<ConflictChoice> {
        let always = if is_dir(incoming) && is_dir(existing) {
            &mut self.folders
        } else {
            &mut self.files
        };
        if let Some(choice) = *always {
            return Some(choice);
        }
        let Answer::Conflict { choice, all } = p.ask(Prompt::Conflict {
            incoming: incoming.to_path_buf(),
            existing: existing.to_path_buf(),
        })?
        else {
            return None;
        };
        if all {
            *always = Some(choice);
        }
        Some(choice)
    }
}

/// A folder, not following symlinks.
fn is_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// What to do with a job's result once it finishes.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Select the new item in tabs showing its folder.
    Reveal(PathBuf),
    /// Open the file in its default app.
    Open(PathBuf),
    /// Open the file with this app.
    OpenWith(PathBuf, crate::open_with::Target),
}

impl Progress {
    pub(crate) fn cancelled(&self) -> bool {
        self.cancel.load(Relaxed)
    }

    pub(crate) fn add_totals(&self, bytes: u64, items: u64) {
        self.total_bytes.fetch_add(bytes, Relaxed);
        self.total_items.fetch_add(items, Relaxed);
    }

    pub(crate) fn add_done(&self, bytes: u64, items: u64) {
        self.done_bytes.fetch_add(bytes, Relaxed);
        self.done_items.fetch_add(items, Relaxed);
    }

    /// For progress measured as a position, e.g. in the archive being read.
    pub(crate) fn set_done_bytes(&self, bytes: u64) {
        self.done_bytes.store(bytes, Relaxed);
    }

    /// Starts counting from scratch, e.g. when retrying with a password.
    pub(crate) fn reset(&self) {
        for counter in [
            &self.total_bytes,
            &self.done_bytes,
            &self.total_items,
            &self.done_items,
        ] {
            counter.store(0, Relaxed);
        }
    }

    pub(crate) fn has_errors(&self) -> bool {
        !self.errors.lock().expect("errors poisoned").is_empty()
    }

    pub(crate) fn set_outcome(&self, outcome: Outcome) {
        *self.outcome.lock().expect("outcome poisoned") = Some(outcome);
    }

    /// Asks the user and waits for the answer; `None` if the job was cancelled.
    pub(crate) fn ask(&self, prompt: Prompt) -> Option<Answer> {
        *self.prompt.lock().expect("prompt poisoned") = Some(prompt);
        let mut answer = self.answer.lock().expect("answer poisoned");
        loop {
            if let Some(answer) = answer.take() {
                return Some(answer);
            }
            if self.cancelled() {
                *self.prompt.lock().expect("prompt poisoned") = None;
                return None;
            }
            // The UI repaints while jobs run, so it shows the prompt promptly.
            answer = self
                .answered
                .wait_timeout(answer, Duration::from_millis(100))
                .expect("answer poisoned")
                .0;
        }
    }

    fn reply(&self, answer: Answer) {
        *self.prompt.lock().expect("prompt poisoned") = None;
        *self.answer.lock().expect("answer poisoned") = Some(answer);
        self.answered.notify_all();
    }

    /// Waits for the worker to ask something and answers it.
    #[cfg(test)]
    pub(crate) fn answer(&self, answer: impl FnOnce(&Prompt) -> Answer) {
        loop {
            if let Some(prompt) = &*self.prompt.lock().expect("prompt poisoned") {
                let answer = answer(prompt);
                *self.answer.lock().expect("answer poisoned") = Some(answer);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        *self.prompt.lock().expect("prompt poisoned") = None;
        self.answered.notify_all();
    }

    #[cfg(test)]
    pub(crate) fn cancel_for_test(&self) {
        self.cancel.store(true, Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn take_outcome(&self) -> Option<Outcome> {
        self.outcome.lock().expect("outcome poisoned").take()
    }

    #[cfg(test)]
    pub(crate) fn errors(&self) -> Vec<String> {
        self.errors.lock().expect("errors poisoned").clone()
    }

    fn waiting(&self) -> bool {
        self.prompt.lock().expect("prompt poisoned").is_some()
    }

    pub(crate) fn error(&self, path: &Path, err: impl std::fmt::Display) {
        let mut errors = self.errors.lock().expect("errors poisoned");
        if errors.len() < MAX_ERRORS {
            errors.push(format!("{}: {err}", path.display()));
        }
    }

    pub(crate) fn set_current(&self, path: &Path) {
        *self.current.lock().expect("current poisoned") = path.display().to_string();
    }

    fn fraction(&self) -> f32 {
        let (done, total) = match self.total_bytes.load(Relaxed) {
            0 => (
                self.done_items.load(Relaxed),
                self.total_items.load(Relaxed),
            ),
            total => (self.done_bytes.load(Relaxed), total),
        };
        if total == 0 {
            0.0
        } else {
            done as f32 / total as f32
        }
    }
}

struct Job {
    title: String,
    progress: Arc<Progress>,
    handle: Option<JoinHandle<()>>,
    finished_at: Option<Instant>,
    prompt_ui: PromptUi,
}

/// What the user has typed or ticked in a job's prompt so far.
#[derive(Default)]
struct PromptUi {
    password: String,
    show_password: bool,
    apply_to_all: bool,
    focus: bool,
}

pub struct Jobs {
    ctx: egui::Context,
    jobs: Vec<Job>,
    outcomes: Vec<Outcome>,
}

impl Jobs {
    pub fn new(ctx: &egui::Context) -> Self {
        Self {
            ctx: ctx.clone(),
            jobs: Vec::new(),
            outcomes: Vec::new(),
        }
    }

    /// Results of finished jobs for the app to act on.
    pub fn take_outcomes(&mut self) -> Vec<Outcome> {
        std::mem::take(&mut self.outcomes)
    }

    pub fn start(&mut self, kind: JobKind) {
        let progress = Arc::new(Progress::default());
        let title = kind.title();
        let worker_progress = progress.clone();
        let ctx = self.ctx.clone();
        let handle = std::thread::Builder::new()
            .name("file-job".to_owned())
            .spawn(move || {
                run(kind, &worker_progress);
                worker_progress.finished.store(true, Relaxed);
                ctx.request_repaint();
            })
            .expect("failed to spawn file job thread");
        self.jobs.push(Job {
            title,
            progress,
            handle: Some(handle),
            finished_at: None,
            prompt_ui: PromptUi::default(),
        });
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub fn running(&self) -> usize {
        self.jobs.iter().filter(|j| j.finished_at.is_none()).count()
    }

    /// Returns true if any job finished since the last call.
    pub fn poll(&mut self) -> bool {
        let now = Instant::now();
        let mut any_finished = false;
        for job in &mut self.jobs {
            if job.finished_at.is_none() && job.progress.finished.load(Relaxed) {
                job.finished_at = Some(now);
                if let Some(handle) = job.handle.take() {
                    let _ = handle.join();
                }
                if let Some(outcome) = job
                    .progress
                    .outcome
                    .lock()
                    .expect("outcome poisoned")
                    .take()
                    && !job.progress.cancelled()
                {
                    self.outcomes.push(outcome);
                }
                any_finished = true;
            }
        }
        // Drop finished jobs without errors after a moment; failures stay until dismissed.
        self.jobs.retain(|job| {
            job.finished_at
                .is_none_or(|t| now - t < LINGER || job.has_errors())
        });
        if self.running() > 0 {
            self.ctx.request_repaint_after(Duration::from_millis(100));
        } else if !self.jobs.is_empty() {
            self.ctx.request_repaint_after(LINGER);
        }
        any_finished
    }

    /// Cancels all jobs and waits for their threads, so partial files get cleaned up.
    pub fn cancel_all_and_wait(&mut self) {
        for job in &self.jobs {
            job.progress.cancel.store(true, Relaxed);
        }
        for job in &mut self.jobs {
            if let Some(handle) = job.handle.take() {
                let _ = handle.join();
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        let mut dismiss = None;
        for (index, job) in self.jobs.iter().enumerate() {
            let p = &job.progress;
            let finished = job.finished_at.is_some();
            let cancelled = p.cancelled();
            let errors = p.errors.lock().expect("errors poisoned").clone();
            ui.horizontal(|ui| {
                let (glyph, status) = match (finished, cancelled, errors.len()) {
                    (false, false, _) if p.waiting() => {
                        (icon::QUESTION, "Waiting for you…".to_owned())
                    }
                    (false, true, _) => (icon::SPINNER_GAP, "Cancelling…".to_owned()),
                    (false, false, _) => (icon::SPINNER_GAP, String::new()),
                    (true, true, _) => (icon::X_CIRCLE, "Cancelled".to_owned()),
                    (true, false, 0) => (icon::CHECK_CIRCLE, "Done".to_owned()),
                    (true, false, n) => (icon::WARNING, format!("{n} error(s)")),
                };
                ui.label(RichText::new(glyph).size(16.0));
                ui.label(&job.title);

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if finished {
                        if ui
                            .add(Button::new(icon::X).frame(false))
                            .on_hover_text("Dismiss")
                            .clicked()
                        {
                            dismiss = Some(index);
                        }
                    } else if ui
                        .add_enabled(!cancelled, Button::new(icon::X).frame(false))
                        .on_hover_text("Cancel")
                        .clicked()
                    {
                        p.cancel.store(true, Relaxed);
                    }

                    if !status.is_empty() {
                        let label = ui.label(&status);
                        if !errors.is_empty() {
                            label.on_hover_text(errors.join("\n"));
                        }
                    }
                    if !finished {
                        ui.add(ProgressBar::new(p.fraction()).desired_width(220.0));
                        let bytes = p.total_bytes.load(Relaxed);
                        let detail = format!(
                            "{} / {}{}",
                            p.done_items.load(Relaxed),
                            p.total_items.load(Relaxed),
                            if bytes > 0 {
                                format!(
                                    " · {} / {}",
                                    human_size(p.done_bytes.load(Relaxed)),
                                    human_size(bytes)
                                )
                            } else {
                                String::new()
                            }
                        );
                        ui.weak(detail);
                        let current = p.current.lock().expect("current poisoned").clone();
                        ui.add(egui::Label::new(RichText::new(current).weak().small()).truncate());
                    } else if let Some(first) = errors.first() {
                        ui.add(
                            egui::Label::new(
                                RichText::new(first).color(ui.visuals().error_fg_color),
                            )
                            .truncate(),
                        );
                    }
                });
            });
        }
        if let Some(index) = dismiss {
            self.jobs.remove(index);
        }
        self.prompt_modal(ui.ctx());
    }

    /// Shows the first question a job is waiting on.
    fn prompt_modal(&mut self, ctx: &egui::Context) {
        let Some(job) = self
            .jobs
            .iter_mut()
            .find(|j| j.finished_at.is_none() && j.progress.waiting())
        else {
            return;
        };
        let p = job.progress.clone();
        let guard = p.prompt.lock().expect("prompt poisoned");
        let Some(prompt) = guard.as_ref() else {
            return;
        };
        let state = &mut job.prompt_ui;
        let mut answer = None;
        let mut cancel = false;
        Modal::new(Id::new(("job-prompt", Arc::as_ptr(&p) as usize))).show(ctx, |ui| {
            ui.set_max_width(440.0);
            match prompt {
                Prompt::Password { archive, wrong } => {
                    ui.heading(format!("{} Password required", icon::LOCK));
                    ui.label(format!("“{}” is encrypted.", display_name(archive)));
                    if *wrong {
                        ui.colored_label(ui.visuals().error_fg_color, "Wrong password, try again.");
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let edit = ui.add(
                            TextEdit::singleline(&mut state.password)
                                .password(!state.show_password)
                                .hint_text("Password")
                                .desired_width(300.0),
                        );
                        if !std::mem::replace(&mut state.focus, true) {
                            edit.request_focus();
                        }
                        let eye = if state.show_password {
                            icon::EYE_SLASH
                        } else {
                            icon::EYE
                        };
                        if ui.button(eye).on_hover_text("Show password").clicked() {
                            state.show_password = !state.show_password;
                        }
                        if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                            answer = Some(Answer::Password(state.password.clone()));
                        }
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Unlock").clicked() {
                            answer = Some(Answer::Password(state.password.clone()));
                        }
                        cancel = ui.button("Cancel").clicked();
                    });
                }
                Prompt::Conflict { incoming, existing } => {
                    let folders = is_dir(incoming) && is_dir(existing);
                    let what = if folders { "Folder" } else { "File" };
                    ui.heading(format!("{} {what} already exists", icon::WARNING));
                    ui.label(format!(
                        "“{}” already exists in “{}”.",
                        display_name(existing),
                        existing.parent().map(display_name).unwrap_or_default()
                    ));
                    ui.add_space(4.0);
                    let describe = |path: &Path| match fs::symlink_metadata(path) {
                        Ok(meta) if meta.is_dir() => {
                            format!("folder, modified {}", format_time(meta.modified().ok()))
                        }
                        Ok(meta) => format!(
                            "{}, modified {}",
                            human_size(meta.len()),
                            format_time(meta.modified().ok())
                        ),
                        Err(_) => String::new(),
                    };
                    egui::Grid::new("conflict-details")
                        .num_columns(2)
                        .show(ui, |ui| {
                            ui.strong("Existing:");
                            ui.label(describe(existing));
                            ui.end_row();
                            ui.strong("New:");
                            ui.label(describe(incoming));
                            ui.end_row();
                        });
                    ui.add_space(4.0);
                    let all = if folders {
                        "Do this for all folders"
                    } else {
                        "Do this for all files"
                    };
                    ui.checkbox(&mut state.apply_to_all, all);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let (overwrite, overwrite_hint) = if folders {
                            ("Merge", "Put its contents in the existing folder, asking about files in both")
                        } else {
                            ("Overwrite", "Replace the existing one")
                        };
                        for (choice, label, hint) in [
                            (ConflictChoice::Overwrite, overwrite, overwrite_hint),
                            (ConflictChoice::Skip, "Skip", "Leave the existing one as it is"),
                            (ConflictChoice::KeepBoth, "Keep both", "Give the new one another name"),
                        ] {
                            if ui.button(label).on_hover_text(hint).clicked() {
                                answer = Some(Answer::Conflict {
                                    choice,
                                    all: state.apply_to_all,
                                });
                            }
                        }
                        ui.separator();
                        cancel = ui.button("Cancel").clicked();
                    });
                }
            }
        });
        drop(guard);
        if let Some(answer) = answer {
            // Ready for the next question.
            state.focus = false;
            p.reply(answer);
        } else if cancel || ctx.input(|i| i.key_pressed(Key::Escape)) {
            // Not on clicks beside the dialog: that would cancel the whole job.
            p.cancel.store(true, Relaxed);
        }
    }
}

impl Job {
    fn has_errors(&self) -> bool {
        self.progress.has_errors()
    }
}

// ---------------------------------------------------------------------------
// Worker

fn run(kind: JobKind, p: &Progress) {
    match kind {
        JobKind::Copy { sources, dest } => {
            if let Err(err) = check_not_into_itself(&sources, &dest) {
                return p.error(&dest, err);
            }
            for src in &sources {
                scan(src, p);
            }
            let mut paste = Paste::new(Transfer::Copy, p);
            for src in &sources {
                let Some(name) = src.file_name() else {
                    continue;
                };
                let target = dest.join(name);
                if target == *src {
                    // Pasted where it came from: a copy beside it.
                    copy_tree(src, &unique_path(&dest, name), p, &mut paste.buffer);
                } else if !paste.place(src, &target) {
                    return;
                }
            }
        }
        JobKind::Move { sources, dest } => {
            if let Err(err) = check_not_into_itself(&sources, &dest) {
                return p.error(&dest, err);
            }
            p.total_items.store(sources.len() as u64, Relaxed);
            // Renames are instant; only cross-device moves need copy + delete.
            let mut paste = Paste::new(Transfer::Move, p);
            for src in &sources {
                let Some(name) = src.file_name() else {
                    continue;
                };
                if src.parent() != Some(dest.as_path()) {
                    p.set_current(src);
                    if !paste.place(src, &dest.join(name)) {
                        return;
                    }
                }
                p.done_items.fetch_add(1, Relaxed);
            }
            paste.finish_moves();
        }
        JobKind::Restore { items } => {
            p.total_items.store(items.len() as u64, Relaxed);
            for item in items {
                if p.cancelled() {
                    return;
                }
                let original = item.original_path();
                p.set_current(&original);
                if let Err(err) = trash::os_limited::restore_all([item]) {
                    p.error(&original, restore_error(err));
                }
                p.done_items.fetch_add(1, Relaxed);
            }
        }
        JobKind::Purge { items } => {
            p.total_items.store(items.len() as u64, Relaxed);
            for item in items {
                if p.cancelled() {
                    return;
                }
                let original = item.original_path();
                p.set_current(&original);
                if let Err(err) = trash::os_limited::purge_all([item]) {
                    p.error(&original, err);
                }
                p.done_items.fetch_add(1, Relaxed);
            }
        }
        JobKind::Eject { drive, .. } => {
            // Lets the tabs that were moved off the drive drop their file
            // watches first.
            std::thread::sleep(std::time::Duration::from_millis(300));
            p.set_current(&drive);
            if let Err(err) = crate::devices::eject(&drive) {
                p.error(&drive, err);
            }
        }
        JobKind::Compress(job) => archive::compress(job, p),
        JobKind::Extract(job) => archive::extract(job, p),
        JobKind::Trash { paths } => {
            p.total_items.store(paths.len() as u64, Relaxed);
            for path in paths {
                if p.cancelled() {
                    return;
                }
                p.set_current(&path);
                if let Err(err) = trash::delete(&path) {
                    p.error(&path, err);
                }
                p.done_items.fetch_add(1, Relaxed);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Transfer {
    Copy,
    Move,
}

/// Puts pasted (or dropped) items in place, asking about names already
/// taken.
struct Paste<'a> {
    how: Transfer,
    p: &'a Progress,
    resolver: Resolver,
    buffer: Vec<u8>,
    /// Moves across drives, done by copying once every rename is: from, to,
    /// and what it replaces once there.
    later: Vec<(PathBuf, PathBuf, Option<PathBuf>)>,
    /// Folders moved out of by merging, removed at the end if empty.
    emptied: Vec<PathBuf>,
}

impl<'a> Paste<'a> {
    fn new(how: Transfer, p: &'a Progress) -> Self {
        Self {
            how,
            p,
            resolver: Resolver::default(),
            buffer: vec![0; BUFFER_SIZE],
            later: Vec::new(),
            emptied: Vec::new(),
        }
    }

    /// Puts `src` at `dst`, asking first if something is there. False once
    /// the job is cancelled.
    fn place(&mut self, src: &Path, dst: &Path) -> bool {
        if self.p.cancelled() {
            return false;
        }
        if fs::symlink_metadata(dst).is_err() {
            self.put(src, dst, None);
            return true;
        }
        let (Some(dir), Some(name)) = (dst.parent(), dst.file_name()) else {
            return true;
        };
        let Some(choice) = self.resolver.choose(src, dst, self.p) else {
            return false;
        };
        match choice {
            ConflictChoice::Skip => self.skip(src),
            ConflictChoice::KeepBoth => self.put(src, &unique_path(dir, name), None),
            ConflictChoice::Overwrite if src.starts_with(dst) => {
                self.p.error(src, "it's inside the folder it would go over");
                self.skip(src);
            }
            ConflictChoice::Overwrite if is_dir(src) && is_dir(dst) => return self.merge(src, dst),
            // Beside it first, so a failed copy leaves the old one alone.
            ConflictChoice::Overwrite => self.put(src, &unique_path(dir, name), Some(dst)),
        }
        true
    }

    /// Places what's in folder `src` into folder `dst`.
    fn merge(&mut self, src: &Path, dst: &Path) -> bool {
        let entries = match fs::read_dir(src) {
            Ok(entries) => entries,
            Err(err) => {
                self.p.error(src, err);
                return true;
            }
        };
        for entry in entries.flatten() {
            if !self.place(&entry.path(), &dst.join(entry.file_name())) {
                return false;
            }
        }
        if self.how == Transfer::Move {
            self.emptied.push(src.to_path_buf());
        }
        true
    }

    /// Copies or moves `src` to the free `dst`, then puts it over `replace`.
    fn put(&mut self, src: &Path, dst: &Path, replace: Option<&Path>) {
        match self.how {
            Transfer::Copy => {
                if copy_tree(src, dst, self.p, &mut self.buffer)
                    && let Some(replace) = replace
                {
                    self.swap(dst, replace);
                }
            }
            Transfer::Move => match fs::rename(src, dst) {
                Ok(()) => {
                    if let Some(replace) = replace {
                        self.swap(dst, replace);
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
                    self.later.push((
                        src.to_path_buf(),
                        dst.to_path_buf(),
                        replace.map(Path::to_path_buf),
                    ));
                }
                Err(err) => self.p.error(src, err),
            },
        }
    }

    /// Replaces `old` with `new`, which is beside it.
    fn swap(&self, new: &Path, old: &Path) {
        if let Err(err) = remove_all(old).and_then(|()| fs::rename(new, old)) {
            self.p.error(
                old,
                format!(
                    "couldn't replace it ({err}); the new one is “{}”",
                    display_name(new)
                ),
            );
        }
    }

    /// Counts a skipped copy as done, so the progress still ends full.
    fn skip(&self, src: &Path) {
        if self.how == Transfer::Copy {
            let (bytes, items) = measure(src, self.p);
            self.p.add_done(bytes, items);
        }
    }

    /// Copies (then deletes) what couldn't be renamed across drives, and
    /// removes the folders merging emptied.
    fn finish_moves(mut self) {
        let later = std::mem::take(&mut self.later);
        if !later.is_empty() {
            // Re-count progress in terms of the files that need copying.
            self.p.reset();
            for (src, _, _) in &later {
                scan(src, self.p);
            }
            for (src, dst, replace) in later {
                if !copy_tree(&src, &dst, self.p, &mut self.buffer) {
                    continue;
                }
                if let Err(err) = remove_all(&src) {
                    self.p.error(&src, err);
                }
                if let Some(replace) = replace {
                    self.swap(&dst, &replace);
                }
            }
        }
        // Innermost first; ones still holding skipped items stay.
        for dir in self.emptied.iter().rev() {
            let _ = fs::remove_dir(dir);
        }
    }
}

fn restore_error(err: trash::Error) -> String {
    match err {
        trash::Error::RestoreCollision { .. } => {
            "something already exists at the original location".to_owned()
        }
        err => err.to_string(),
    }
}

fn check_not_into_itself(sources: &[PathBuf], dest: &Path) -> Result<(), String> {
    match sources
        .iter()
        .find(|src| src.is_dir() && dest.starts_with(src))
    {
        Some(src) => Err(format!("cannot paste {} into itself", src.display())),
        None => Ok(()),
    }
}

/// Adds the files and bytes under `path` to the totals.
pub(crate) fn scan(path: &Path, p: &Progress) {
    let (bytes, items) = measure(path, p);
    p.add_totals(bytes, items);
}

/// The bytes and files under `path`, as progress counts them.
fn measure(path: &Path, p: &Progress) -> (u64, u64) {
    if p.cancelled() {
        return (0, 0);
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return (0, 0);
    };
    if meta.is_dir() {
        fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| measure(&entry.path(), p))
            .fold((0, 0), |(b, i), (eb, ei)| (b + eb, i + ei))
    } else {
        (if meta.is_file() { meta.len() } else { 0 }, 1)
    }
}

/// Recursively copies `src` to `dst`. Returns true if everything was copied.
pub(crate) fn copy_tree(src: &Path, dst: &Path, p: &Progress, buffer: &mut [u8]) -> bool {
    if p.cancelled() {
        return false;
    }
    let meta = match fs::symlink_metadata(src) {
        Ok(meta) => meta,
        Err(err) => {
            p.error(src, err);
            return false;
        }
    };

    #[cfg(unix)]
    if meta.file_type().is_symlink() {
        let result = fs::read_link(src).and_then(|target| std::os::unix::fs::symlink(target, dst));
        p.done_items.fetch_add(1, Relaxed);
        return match result {
            Ok(()) => true,
            Err(err) => {
                p.error(src, err);
                false
            }
        };
    }

    if meta.is_dir() {
        if let Err(err) = fs::create_dir(dst) {
            p.error(dst, err);
            return false;
        }
        let entries = match fs::read_dir(src) {
            Ok(entries) => entries,
            Err(err) => {
                p.error(src, err);
                return false;
            }
        };
        let mut ok = true;
        for entry in entries {
            match entry {
                Ok(entry) => {
                    ok &= copy_tree(&entry.path(), &dst.join(entry.file_name()), p, buffer)
                }
                Err(err) => {
                    p.error(src, err);
                    ok = false;
                }
            }
        }
        let _ = fs::set_permissions(dst, meta.permissions());
        ok
    } else {
        copy_file(src, dst, &meta, p, buffer)
    }
}

fn copy_file(src: &Path, dst: &Path, meta: &fs::Metadata, p: &Progress, buffer: &mut [u8]) -> bool {
    p.set_current(src);
    let result = (|| -> io::Result<()> {
        let mut from = fs::File::open(src)?;
        let mut to = fs::File::create_new(dst)?;
        loop {
            if p.cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let n = from.read(buffer)?;
            if n == 0 {
                break;
            }
            to.write_all(&buffer[..n])?;
            p.done_bytes.fetch_add(n as u64, Relaxed);
        }
        if let Ok(modified) = meta.modified() {
            let _ = to.set_modified(modified);
        }
        to.set_permissions(meta.permissions())
    })();
    p.done_items.fetch_add(1, Relaxed);
    match result {
        Ok(()) => true,
        Err(err) => {
            // Don't leave a partial file behind.
            let _ = fs::remove_file(dst);
            if !p.cancelled() {
                p.error(src, err);
            }
            false
        }
    }
}

pub(crate) fn remove_all(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(jobs: &mut Jobs) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while jobs.running() > 0 {
            assert!(Instant::now() < deadline, "job timed out");
            jobs.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn errors(jobs: &Jobs) -> Vec<String> {
        jobs.jobs
            .iter()
            .flat_map(|j| j.progress.errors.lock().unwrap().clone())
            .collect()
    }

    /// Runs `kind`, answering each of its questions with `answer`; the
    /// existing names it was asked about, and the errors.
    fn run_answering(
        kind: JobKind,
        answer: impl Fn(&Path) -> ConflictChoice,
        all: bool,
    ) -> (Vec<String>, Vec<String>) {
        let p = Arc::new(Progress::default());
        let worker = {
            let p = p.clone();
            std::thread::spawn(move || run(kind, &p))
        };
        let mut asked = Vec::new();
        while !worker.is_finished() {
            if let Some(Prompt::Conflict { existing, .. }) = &*p.prompt.lock().unwrap() {
                asked.push(display_name(existing));
                *p.answer.lock().unwrap() = Some(Answer::Conflict {
                    choice: answer(existing),
                    all,
                });
            } else {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            *p.prompt.lock().unwrap() = None;
            p.answered.notify_all();
        }
        worker.join().unwrap();
        let errors = p.errors();
        (asked, errors)
    }

    fn scratch(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("egui-files-jobs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn read(path: PathBuf) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn copy_and_move_trees() {
        let root = scratch("trees");
        let (src, dest) = (root.join("src"), root.join("dest"));
        fs::create_dir_all(src.join("tree/sub")).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(src.join("tree/a.txt"), "a").unwrap();
        fs::write(src.join("tree/sub/b.bin"), vec![7u8; 3 * BUFFER_SIZE + 5]).unwrap();

        let mut jobs = Jobs::new(&egui::Context::default());
        jobs.start(JobKind::Copy {
            sources: vec![src.join("tree")],
            dest: dest.clone(),
        });
        wait(&mut jobs);
        assert_eq!(errors(&jobs), Vec::<String>::new());
        assert_eq!(
            fs::read(dest.join("tree/sub/b.bin")).unwrap().len(),
            3 * BUFFER_SIZE + 5
        );
        let p = &jobs.jobs[0].progress;
        assert_eq!(p.done_items.load(Relaxed), p.total_items.load(Relaxed));
        assert_eq!(p.fraction(), 1.0);

        // Pasting where it came from makes a copy beside it, unasked.
        jobs.start(JobKind::Copy {
            sources: vec![dest.join("tree")],
            dest: dest.clone(),
        });
        wait(&mut jobs);
        assert!(dest.join("tree (1)/a.txt").exists());

        jobs.start(JobKind::Copy {
            sources: vec![dest.clone()],
            dest: dest.join("tree"),
        });
        wait(&mut jobs);
        assert!(errors(&jobs).iter().any(|e| e.contains("into itself")));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn paste_asks_about_names_taken() {
        let root = scratch("conflicts");
        let (src, dest) = (root.join("src"), root.join("dest"));
        for dir in [&src, &dest] {
            fs::create_dir_all(dir.join("tree")).unwrap();
            fs::write(dir.join("single.txt"), format!("{}", dir.display())).unwrap();
            fs::write(dir.join("tree/both.txt"), format!("{}", dir.display())).unwrap();
        }
        fs::write(src.join("tree/new.txt"), "new").unwrap();
        fs::write(dest.join("tree/mine.txt"), "mine").unwrap();
        let src_text = src.display().to_string();
        let dest_text = dest.display().to_string();
        let copy = || JobKind::Copy {
            sources: vec![src.join("single.txt"), src.join("tree")],
            dest: dest.clone(),
        };

        // Skip touches nothing.
        let (asked, errors) = run_answering(copy(), |_| ConflictChoice::Skip, false);
        assert_eq!(asked, ["single.txt", "tree"]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(read(dest.join("single.txt")), dest_text);
        assert!(!dest.join("tree/new.txt").exists());

        // Keep both.
        let (_, errors) = run_answering(copy(), |_| ConflictChoice::KeepBoth, false);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(read(dest.join("single (1).txt")), src_text);
        assert_eq!(read(dest.join("tree (1)/both.txt")), src_text);
        fs::remove_dir_all(dest.join("tree (1)")).unwrap();
        fs::remove_file(dest.join("single (1).txt")).unwrap();

        // Overwrite the file and merge the folder, asking about the file in
        // both, which is kept.
        let (asked, errors) = run_answering(
            copy(),
            |existing| {
                if existing.ends_with("tree/both.txt") {
                    ConflictChoice::Skip
                } else {
                    ConflictChoice::Overwrite
                }
            },
            false,
        );
        assert_eq!(asked, ["single.txt", "tree", "both.txt"]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(read(dest.join("single.txt")), src_text);
        assert_eq!(read(dest.join("tree/both.txt")), dest_text);
        assert_eq!(read(dest.join("tree/new.txt")), "new");
        assert_eq!(read(dest.join("tree/mine.txt")), "mine");
        assert!(
            !dest.join("single (1).txt").exists(),
            "no copy left beside it"
        );

        // Moving, merged and overwritten for all: the sources go.
        fs::write(dest.join("tree/both.txt"), "old").unwrap();
        let (asked, errors) = run_answering(
            JobKind::Move {
                sources: vec![src.join("single.txt"), src.join("tree")],
                dest: dest.clone(),
            },
            |_| ConflictChoice::Overwrite,
            true,
        );
        assert_eq!(
            asked,
            ["single.txt", "tree"],
            "files asked once, folders once"
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(read(dest.join("tree/both.txt")), src_text);
        assert_eq!(read(dest.join("tree/mine.txt")), "mine");
        assert!(!src.join("tree").exists());
        assert!(!src.join("single.txt").exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn overwriting_a_folder_with_something_inside_it_fails() {
        let root = scratch("inside");
        fs::create_dir_all(root.join("x/x")).unwrap();
        fs::write(root.join("x/x/keep.txt"), "keep").unwrap();
        let (_, errors) = run_answering(
            JobKind::Move {
                sources: vec![root.join("x/x")],
                dest: root.clone(),
            },
            |_| ConflictChoice::Overwrite,
            false,
        );
        assert!(
            errors.iter().any(|e| e.contains("inside the folder")),
            "{errors:?}"
        );
        assert_eq!(read(root.join("x/x/keep.txt")), "keep");

        fs::create_dir_all(root.join("y")).unwrap();
        fs::write(root.join("y/y"), "file").unwrap();
        let (_, errors) = run_answering(
            JobKind::Copy {
                sources: vec![root.join("y/y")],
                dest: root.clone(),
            },
            |_| ConflictChoice::Overwrite,
            false,
        );
        assert!(
            errors.iter().any(|e| e.contains("inside the folder")),
            "{errors:?}"
        );
        assert_eq!(read(root.join("y/y")), "file");

        fs::remove_dir_all(root).unwrap();
    }

    /// Moves between two filesystems. Opt-in, since it needs two writable folders
    /// on different devices:
    ///
    /// `EGUI_FILES_DIR_A=/mnt/x EGUI_FILES_DIR_B=/mnt/y cargo test cross_device -- --ignored`
    #[test]
    #[ignore]
    #[cfg(unix)]
    fn cross_device_move() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = |var| PathBuf::from(std::env::var(var).unwrap_or_else(|_| panic!("set {var}")));
        let name = format!("egui-files-xdev-{}", std::process::id());
        let (src, dest) = (
            dir("EGUI_FILES_DIR_A").join(&name),
            dir("EGUI_FILES_DIR_B").join(&name),
        );
        let _ = (fs::remove_dir_all(&src), fs::remove_dir_all(&dest));
        fs::create_dir_all(src.join("tree/sub")).unwrap();
        fs::create_dir_all(&dest).unwrap();
        assert_ne!(
            fs::metadata(&src).unwrap().dev(),
            fs::metadata(&dest).unwrap().dev(),
            "folders must be on different filesystems"
        );

        let small = src.join("tree/a.txt");
        fs::write(&small, "hello").unwrap();
        fs::set_permissions(&small, fs::Permissions::from_mode(0o640)).unwrap();
        let mtime = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        fs::File::options()
            .write(true)
            .open(&small)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let big: Vec<u8> = (0..5 * BUFFER_SIZE + 123)
            .map(|i| (i % 251) as u8)
            .collect();
        fs::write(src.join("tree/sub/big.bin"), &big).unwrap();
        std::os::unix::fs::symlink("a.txt", src.join("tree/link")).unwrap();

        let mut jobs = Jobs::new(&egui::Context::default());
        jobs.start(JobKind::Move {
            sources: vec![src.join("tree")],
            dest: dest.clone(),
        });
        wait(&mut jobs);
        assert_eq!(errors(&jobs), Vec::<String>::new());
        assert!(!src.join("tree").exists(), "source removed after move");
        let moved = dest.join("tree");
        assert_eq!(fs::read(moved.join("sub/big.bin")).unwrap(), big);
        let meta = fs::metadata(moved.join("a.txt")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o640);
        assert_eq!(meta.modified().unwrap(), mtime);
        assert_eq!(
            fs::read_link(moved.join("link")).unwrap(),
            Path::new("a.txt")
        );
        let p = &jobs.jobs[0].progress;
        assert_eq!(p.done_bytes.load(Relaxed), p.total_bytes.load(Relaxed));
        assert_eq!(p.done_items.load(Relaxed), p.total_items.load(Relaxed));

        // Cancelling right away must never lose data: exactly one complete copy remains.
        let huge = src.join("huge.bin");
        let size = 64 * BUFFER_SIZE as u64;
        fs::File::create(&huge).unwrap().set_len(size).unwrap();
        jobs.start(JobKind::Move {
            sources: vec![huge.clone()],
            dest: dest.clone(),
        });
        jobs.jobs
            .last()
            .unwrap()
            .progress
            .cancel
            .store(true, Relaxed);
        wait(&mut jobs);
        let len = |path: &Path| fs::metadata(path).map(|m| m.len()).ok();
        match (len(&huge), len(&dest.join("huge.bin"))) {
            (Some(n), None) | (None, Some(n)) => assert_eq!(n, size),
            other => panic!("expected exactly one complete copy, got {other:?}"),
        }

        fs::remove_dir_all(src).unwrap();
        fs::remove_dir_all(dest).unwrap();
    }
}
