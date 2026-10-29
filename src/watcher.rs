//! Watches the folders on screen and reports which ones changed, debounced.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher, event::ModifyKind};

/// Wait this long after the last event before refreshing...
const QUIET: Duration = Duration::from_millis(200);
/// ...but refresh at least this often during a burst (e.g. a large copy).
const MAX_DELAY: Duration = Duration::from_millis(1000);

pub struct DirWatcher {
    /// `None` if the platform watcher could not be created; refreshes are then manual.
    watcher: Option<RecommendedWatcher>,
    events: Receiver<notify::Result<notify::Event>>,
    watched: HashSet<PathBuf>,
    dirty: HashSet<PathBuf>,
    first_event: Option<Instant>,
    last_event: Option<Instant>,
    ctx: egui::Context,
}

impl DirWatcher {
    pub fn new(ctx: &egui::Context) -> Self {
        let (sender, events) = mpsc::channel();
        let repaint_ctx = ctx.clone();
        let watcher = notify::recommended_watcher(move |event| {
            if sender.send(event).is_ok() {
                repaint_ctx.request_repaint_after(QUIET);
            }
        })
        .ok();
        Self {
            watcher,
            events,
            watched: HashSet::new(),
            dirty: HashSet::new(),
            first_event: None,
            last_event: None,
            ctx: ctx.clone(),
        }
    }

    /// Watches exactly `dirs` (non-recursively).
    pub fn set_watched(&mut self, dirs: HashSet<PathBuf>) {
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        for dir in self.watched.difference(&dirs) {
            let _ = watcher.unwatch(dir);
        }
        for dir in dirs.difference(&self.watched) {
            // Failures (e.g. unsupported filesystems, inotify limits) just mean
            // that folder needs a manual refresh.
            let _ = watcher.watch(dir, RecursiveMode::NonRecursive);
        }
        self.watched = dirs;
    }

    /// Folders whose contents changed since the last call, once events have settled.
    pub fn poll(&mut self) -> HashSet<PathBuf> {
        let now = Instant::now();
        for event in self.events.try_iter().flatten() {
            // Our own reads (listings, thumbnails, previews) cause access and
            // atime events; reacting to them would refresh in a loop.
            if matches!(
                event.kind,
                EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_))
            ) {
                continue;
            }
            for path in event.paths {
                if let Some(parent) = path.parent() {
                    self.dirty.insert(parent.to_path_buf());
                }
                self.dirty.insert(path);
            }
            self.first_event.get_or_insert(now);
            self.last_event = Some(now);
        }

        let (Some(first), Some(last)) = (self.first_event, self.last_event) else {
            return HashSet::new();
        };
        let due = (last + QUIET).min(first + MAX_DELAY);
        if now < due {
            self.ctx.request_repaint_after(due - now);
            return HashSet::new();
        }
        self.first_event = None;
        self.last_event = None;
        std::mem::take(&mut self.dirty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_changed_dirs_and_ignores_reads() {
        let dir = std::env::temp_dir().join(format!("egui-files-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("existing.txt"), "x").unwrap();

        let mut watcher = DirWatcher::new(&egui::Context::default());
        assert!(watcher.watcher.is_some(), "platform watcher available");
        watcher.set_watched(HashSet::from([dir.clone()]));

        let poll_for = |watcher: &mut DirWatcher, duration: Duration| {
            let deadline = Instant::now() + duration;
            let mut changed = HashSet::new();
            while Instant::now() < deadline && changed.is_empty() {
                changed = watcher.poll();
                std::thread::sleep(Duration::from_millis(20));
            }
            changed
        };

        // Reading the folder and a file must not count as a change.
        let _ = std::fs::read_dir(&dir).unwrap().count();
        let _ = std::fs::read(dir.join("existing.txt")).unwrap();
        assert!(poll_for(&mut watcher, Duration::from_millis(600)).is_empty());

        std::fs::write(dir.join("new.txt"), "y").unwrap();
        assert!(poll_for(&mut watcher, Duration::from_secs(5)).contains(&dir));

        watcher.set_watched(HashSet::new());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
