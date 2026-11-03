//! Archive listings read on background threads, for the preview pane and
//! archive tabs. Compressed tars have to be decompressed to be listed, so this
//! can take a while.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use super::{Error, Listing, read};

/// How many archives' listings are kept.
const CAPACITY: usize = 16;

#[derive(Clone)]
pub enum ListState {
    Loading,
    Ready(Arc<Listing>),
    /// The listing itself is encrypted (7z with encrypted names, some rar).
    NeedsPassword {
        wrong: bool,
    },
    Failed(String),
}

struct Slot {
    modified: Option<SystemTime>,
    password: Option<String>,
    state: Arc<Mutex<ListState>>,
    last_used: u64,
}

pub struct Listings {
    ctx: egui::Context,
    slots: HashMap<PathBuf, Slot>,
    clock: u64,
}

impl Listings {
    pub fn new(ctx: &egui::Context) -> Self {
        Self {
            ctx: ctx.clone(),
            slots: HashMap::new(),
            clock: 0,
        }
    }

    /// The listing of `path`, starting to read it if needed. Re-reads it if
    /// the file changed.
    pub fn get(&mut self, path: &Path) -> ListState {
        let modified = disk::metadata(path).and_then(|m| m.modified()).ok();
        self.clock += 1;
        match self.slots.get_mut(path) {
            Some(slot) if slot.modified == modified => {
                slot.last_used = self.clock;
                slot.state.lock().expect("listing poisoned").clone()
            }
            old => {
                let password = old.and_then(|slot| slot.password.take());
                self.load(path, modified, password)
            }
        }
    }

    /// The password the listing was read with, for extracting.
    pub fn password(&self, path: &Path) -> Option<String> {
        self.slots.get(path)?.password.clone()
    }

    /// Drops the listing, so it's read again next time.
    pub fn forget(&mut self, path: &Path) {
        self.slots.remove(path);
    }

    /// Reads the listing again with a password.
    pub fn unlock(&mut self, path: &Path, password: String) {
        let modified = disk::metadata(path).and_then(|m| m.modified()).ok();
        self.load(path, modified, Some(password));
    }

    fn load(
        &mut self,
        path: &Path,
        modified: Option<SystemTime>,
        password: Option<String>,
    ) -> ListState {
        if self.slots.len() >= CAPACITY
            && let Some(oldest) = self
                .slots
                .iter()
                .min_by_key(|(_, slot)| slot.last_used)
                .map(|(path, _)| path.clone())
        {
            self.slots.remove(&oldest);
        }
        let state = Arc::new(Mutex::new(ListState::Loading));
        self.slots.insert(
            path.to_path_buf(),
            Slot {
                modified,
                password: password.clone(),
                state: state.clone(),
                last_used: self.clock,
            },
        );
        let path = path.to_path_buf();
        let ctx = self.ctx.clone();
        crate::background::spawn("archive-listing", move || {
            let result = match read::list(&path, password.as_deref()) {
                Ok(listing) => ListState::Ready(Arc::new(listing)),
                Err(Error::NeedsPassword) => ListState::NeedsPassword { wrong: false },
                Err(Error::WrongPassword) => ListState::NeedsPassword { wrong: true },
                Err(err) => ListState::Failed(err.to_string()),
            };
            *state.lock().expect("listing poisoned") = result;
            ctx.request_repaint();
        });
        ListState::Loading
    }
}
