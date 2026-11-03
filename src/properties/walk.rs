//! Counting what's inside folders, on a background thread.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    time::Duration,
};

use web_time::Instant;

#[derive(Default)]
struct State {
    files: AtomicU64,
    dirs: AtomicU64,
    bytes: AtomicU64,
    allocated: AtomicU64,
    /// Folders that couldn't be read.
    unreadable: AtomicU64,
    done: AtomicBool,
    cancel: AtomicBool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// Space on disk, where the platform says (0 otherwise).
    pub allocated: u64,
    pub unreadable: u64,
    pub done: bool,
}

/// Counts the contents of some folders (not the folders themselves). Stops
/// when dropped.
pub struct Walk {
    state: Arc<State>,
}

impl Walk {
    pub fn start(ctx: &egui::Context, roots: Vec<PathBuf>) -> Self {
        let state = Arc::new(State::default());
        let (s, ctx) = (state.clone(), ctx.clone());
        crate::background::spawn("properties-walk", move || {
            walk(&s, roots, &ctx);
            s.done.store(true, Relaxed);
            ctx.request_repaint();
        });
        Self { state }
    }

    pub fn counts(&self) -> Counts {
        let s = &self.state;
        Counts {
            files: s.files.load(Relaxed),
            dirs: s.dirs.load(Relaxed),
            bytes: s.bytes.load(Relaxed),
            allocated: s.allocated.load(Relaxed),
            unreadable: s.unreadable.load(Relaxed),
            done: s.done.load(Relaxed),
        }
    }
}

impl Drop for Walk {
    fn drop(&mut self) {
        self.state.cancel.store(true, Relaxed);
    }
}

#[cfg(unix)]
fn device(meta: &disk::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::dev(meta)
}

#[cfg(not(unix))]
fn device(_meta: &disk::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn allocated(meta: &disk::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::blocks(meta) * 512
}

#[cfg(not(unix))]
fn allocated(_meta: &disk::Metadata) -> u64 {
    0
}

fn walk(state: &State, roots: Vec<PathBuf>, ctx: &egui::Context) {
    let mut last_repaint = Instant::now();
    for root in roots {
        // Stay on the root's filesystem, like `du -x`: no wandering into
        // /proc or a network share mounted inside.
        let Ok(root_meta) = disk::metadata(&root) else {
            continue;
        };
        let dev = device(&root_meta);
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            if state.cancel.load(Relaxed) {
                return;
            }
            let Ok(entries) = disk::read_dir(&dir) else {
                state.unreadable.fetch_add(1, Relaxed);
                continue;
            };
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    state.dirs.fetch_add(1, Relaxed);
                    if device(&meta) == dev {
                        stack.push(entry.path());
                    }
                } else {
                    state.files.fetch_add(1, Relaxed);
                    state.bytes.fetch_add(meta.len(), Relaxed);
                }
                state.allocated.fetch_add(allocated(&meta), Relaxed);
            }
            if last_repaint.elapsed() > Duration::from_millis(150) {
                last_repaint = Instant::now();
                ctx.request_repaint();
            }
        }
    }
}
