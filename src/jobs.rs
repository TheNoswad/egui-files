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
                    ui.heading(format!("{} File already exists", icon::WARNING));
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
                    ui.checkbox(&mut state.apply_to_all, "Do this for all conflicts");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        for (choice, label) in [
                            (ConflictChoice::Overwrite, "Overwrite"),
                            (ConflictChoice::Skip, "Skip"),
                            (ConflictChoice::KeepBoth, "Keep both"),
                        ] {
                            if ui.button(label).clicked() {
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
            let mut buffer = vec![0; BUFFER_SIZE];
            for src in &sources {
                let Some(name) = src.file_name() else {
                    continue;
                };
                copy_tree(src, &unique_path(&dest, name), p, &mut buffer);
            }
        }
        JobKind::Move { sources, dest } => {
            if let Err(err) = check_not_into_itself(&sources, &dest) {
                return p.error(&dest, err);
            }
            p.total_items.store(sources.len() as u64, Relaxed);
            // Renames are instant; only cross-device moves need copy + delete.
            let mut fallback = Vec::new();
            for src in &sources {
                if p.cancelled() {
                    return;
                }
                let Some(name) = src.file_name() else {
                    continue;
                };
                if src.parent() == Some(dest.as_path()) {
                    p.done_items.fetch_add(1, Relaxed);
                    continue;
                }
                let target = unique_path(&dest, name);
                p.set_current(src);
                match fs::rename(src, &target) {
                    Ok(()) => {
                        p.done_items.fetch_add(1, Relaxed);
                    }
                    Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
                        fallback.push((src.clone(), target));
                    }
                    Err(err) => p.error(src, err),
                }
            }
            if fallback.is_empty() {
                return;
            }
            // Re-count progress in terms of the files that need copying.
            p.total_items.fetch_sub(fallback.len() as u64, Relaxed);
            for (src, _) in &fallback {
                scan(src, p);
            }
            let mut buffer = vec![0; BUFFER_SIZE];
            for (src, target) in fallback {
                if copy_tree(&src, &target, p, &mut buffer)
                    && let Err(err) = remove_all(&src)
                {
                    p.error(&src, err);
                }
            }
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
    if p.cancelled() {
        return;
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_dir() {
        for entry in fs::read_dir(path).into_iter().flatten().flatten() {
            scan(&entry.path(), p);
        }
    } else {
        p.total_items.fetch_add(1, Relaxed);
        if meta.is_file() {
            p.total_bytes.fetch_add(meta.len(), Relaxed);
        }
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

    #[test]
    fn copy_and_move_trees_with_conflicts() {
        let root = std::env::temp_dir().join(format!("egui-files-jobs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (src, dest) = (root.join("src"), root.join("dest"));
        fs::create_dir_all(src.join("tree/sub")).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(src.join("tree/a.txt"), "a").unwrap();
        fs::write(src.join("tree/sub/b.bin"), vec![7u8; 3 * BUFFER_SIZE + 5]).unwrap();
        fs::write(src.join("single.txt"), "single").unwrap();
        fs::write(dest.join("single.txt"), "existing").unwrap();

        let mut jobs = Jobs::new(&egui::Context::default());
        jobs.start(JobKind::Copy {
            sources: vec![src.join("tree"), src.join("single.txt")],
            dest: dest.clone(),
        });
        wait(&mut jobs);
        assert_eq!(errors(&jobs), Vec::<String>::new());
        assert_eq!(
            fs::read(dest.join("tree/sub/b.bin")).unwrap().len(),
            3 * BUFFER_SIZE + 5
        );
        assert_eq!(
            fs::read_to_string(dest.join("single (1).txt")).unwrap(),
            "single"
        );
        assert_eq!(
            fs::read_to_string(dest.join("single.txt")).unwrap(),
            "existing"
        );
        let p = &jobs.jobs[0].progress;
        assert_eq!(p.done_items.load(Relaxed), p.total_items.load(Relaxed));
        assert_eq!(p.fraction(), 1.0);

        jobs.start(JobKind::Move {
            sources: vec![src.join("tree")],
            dest: dest.clone(),
        });
        wait(&mut jobs);
        assert!(!src.join("tree").exists());
        assert!(dest.join("tree (1)/a.txt").exists());

        jobs.start(JobKind::Copy {
            sources: vec![dest.clone()],
            dest: dest.join("tree"),
        });
        wait(&mut jobs);
        assert!(errors(&jobs).iter().any(|e| e.contains("into itself")));

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
