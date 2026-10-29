//! Long-running file operations (copy, move, trash) on background threads,
//! with progress, cancellation, and a status bar UI.

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use egui::{Button, ProgressBar, RichText, Ui};
use egui_phosphor::regular as icon;

use crate::fs::{display_name, human_size, unique_path};

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
}

impl Progress {
    fn cancelled(&self) -> bool {
        self.cancel.load(Relaxed)
    }

    fn error(&self, path: &Path, err: impl std::fmt::Display) {
        let mut errors = self.errors.lock().expect("errors poisoned");
        if errors.len() < MAX_ERRORS {
            errors.push(format!("{}: {err}", path.display()));
        }
    }

    fn set_current(&self, path: &Path) {
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
}

pub struct Jobs {
    ctx: egui::Context,
    jobs: Vec<Job>,
}

impl Jobs {
    pub fn new(ctx: &egui::Context) -> Self {
        Self {
            ctx: ctx.clone(),
            jobs: Vec::new(),
        }
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
    }
}

impl Job {
    fn has_errors(&self) -> bool {
        !self
            .progress
            .errors
            .lock()
            .expect("errors poisoned")
            .is_empty()
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
fn scan(path: &Path, p: &Progress) {
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
fn copy_tree(src: &Path, dst: &Path, p: &Progress, buffer: &mut [u8]) -> bool {
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

fn remove_all(path: &Path) -> io::Result<()> {
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
}
