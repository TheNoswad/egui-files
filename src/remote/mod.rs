//! Folders on servers (`sftp://…` paths; see `disk::sftp`): listing them
//! off the UI's thread, and asking the user what ssh wants to know.
//!
//! The UI's thread never connects, nor waits on a server for a listing:
//! [`listing`] says whether a folder's ready to read, and if not, fetches it
//! in the background and repaints once it's in. A path whose first part is
//! `~` (`sftp://host/~/notes`) means the user's home folder there, which is
//! only known once connected.

mod connect;
#[cfg(not(target_arch = "wasm32"))]
pub mod copies;
pub mod hosts;
#[cfg(not(target_arch = "wasm32"))]
mod prompts;

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

pub use connect::open as open_connect_dialog;

/// The most servers remembered as recently used.
const MAX_RECENT: usize = 8;

/// Why servers can't be connected to here, if they can't.
pub fn available() -> Result<(), &'static str> {
    if cfg!(target_arch = "wasm32") {
        Err("Connecting to servers needs the desktop app: a web page can't reach them.")
    } else if cfg!(target_os = "android") {
        Err("Connecting to servers isn't available on Android yet.")
    } else {
        Ok(())
    }
}

static RECENT: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Things gone wrong in the background, to tell the user about.
static NOTICES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// How often the servers' folders on screen are listed again, for changes
/// others made there.
#[cfg(not(target_arch = "wasm32"))]
const LOOK_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(not(target_arch = "wasm32"))]
static WATCHED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Looks at these servers' folders (those on screen) every so often, for
/// changes others made there: nothing tells of them otherwise. A change
/// reaches the tabs as the file watcher's would. Only while connected, so
/// it never asks the user anything.
pub fn watch(dirs: Vec<PathBuf>) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        static LOOKING: std::sync::Once = std::sync::Once::new();
        let any = !dirs.is_empty();
        *WATCHED.lock().unwrap_or_else(PoisonError::into_inner) = dirs;
        if any {
            LOOKING.call_once(|| {
                crate::background::spawn("look at servers' folders", || {
                    loop {
                        std::thread::sleep(LOOK_EVERY);
                        let dirs = WATCHED
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .clone();
                        for dir in dirs {
                            if matches!(disk::sftp::relist(&dir), Ok(true)) {
                                repaint();
                            }
                        }
                    }
                });
            });
        }
    }
    #[cfg(target_arch = "wasm32")]
    let _ = dirs;
}

/// Draws the next frame, from any thread: something from a server is in.
pub fn repaint() {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

/// Tells the user something went wrong, from any thread.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn notify(message: String) {
    NOTICES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(message);
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

fn notices_ui(ctx: &egui::Context) {
    let first = NOTICES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .first()
        .cloned();
    let Some(message) = first else { return };
    let mut done = false;
    let modal = egui::Modal::new(egui::Id::new("server_notice")).show(ctx, |ui| {
        ui.set_max_width(crate::widgets::modal_width(ui, 480.0));
        ui.label(message);
        ui.add_space(8.0);
        done = ui.button("OK").clicked();
    });
    if done || modal.should_close() {
        NOTICES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(0);
    }
}

/// Servers connected to lately, as `user@host:port`, latest first.
pub fn recent() -> Vec<String> {
    RECENT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Puts back the servers remembered from last time.
pub fn set_recent(servers: Vec<String>) {
    *RECENT.lock().unwrap_or_else(PoisonError::into_inner) = servers;
}

/// Forgets a server was used, for the places pane's "Forget".
pub fn forget_recent(authority: &str) {
    RECENT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|s| s != authority);
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn remember(authority: &str) {
    // Phones are listed with the drives, and mounted on demand.
    if crate::phones::is_phone_authority(authority) {
        return;
    }
    let mut recent = RECENT.lock().unwrap_or_else(PoisonError::into_inner);
    recent.retain(|s| s != authority);
    recent.insert(0, authority.to_owned());
    recent.truncate(MAX_RECENT);
}

/// The computer `path` is on, as its path names it (`user@host:port`).
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn authority(path: &Path) -> Option<String> {
    #[cfg(not(target_arch = "wasm32"))]
    return disk::sftp::parse(path).map(|location| location.authority);
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        None
    }
}

/// Shows "Connect to server" if it's open; returns where to go once the
/// user says.
pub fn connect_dialog_ui(ctx: &egui::Context) -> Option<PathBuf> {
    connect::ui(ctx)
}

/// How the connection to a server is, for the places pane.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
}

/// Each server connected to, or tried, this session, by `user@host:port`.
pub fn connections() -> Vec<(String, Status)> {
    #[cfg(not(target_arch = "wasm32"))]
    return disk::sftp::connections()
        .into_iter()
        .filter_map(|(root, status)| {
            let status = match status {
                disk::sftp::Status::Disconnected => Status::Disconnected,
                disk::sftp::Status::Connecting => Status::Connecting,
                disk::sftp::Status::Connected => Status::Connected,
                disk::sftp::Status::Failed(why) => Status::Failed(why),
            };
            let authority = authority(&root)?;
            (!crate::phones::is_phone_authority(&authority)).then_some((authority, status))
        })
        .collect();
    #[cfg(target_arch = "wasm32")]
    Vec::new()
}

/// Closes the connection to a server.
pub fn disconnect(authority: &str) {
    #[cfg(not(target_arch = "wasm32"))]
    disk::sftp::disconnect(&home_of(authority));
    #[cfg(target_arch = "wasm32")]
    let _ = authority;
}

/// Whether `path` is on a server.
pub fn is_remote(path: &Path) -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    return disk::sftp::is_remote(path);
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        false
    }
}

/// The top of the server `path` is on (`sftp://host`), if it's on one.
pub fn root(path: &Path) -> Option<PathBuf> {
    #[cfg(not(target_arch = "wasm32"))]
    return disk::sftp::root(path);
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        None
    }
}

/// The user's home folder on the server `authority` names (`user@host`),
/// once connected.
pub fn home_of(authority: &str) -> PathBuf {
    PathBuf::from(format!("sftp://{authority}/~"))
}

/// What a path names when typed: `sftp://host` alone means its home folder.
pub fn typed(path: PathBuf) -> PathBuf {
    match path.to_str() {
        Some(text) if is_remote(&path) => {
            let rest = &text["sftp://".len()..];
            if rest.contains(['/', '\\']) {
                path
            } else {
                home_of(rest)
            }
        }
        _ => path,
    }
}

/// `path` and the folders it's in, up to the top of its drive, server or
/// phone's storage.
pub fn ancestors(path: &Path) -> impl Iterator<Item = &Path> {
    // A phone's first: it's reached like a server, whose top is this
    // computer's, not the phone's.
    let top = crate::phones::root_of(path).or_else(|| root(path));
    let mut done = false;
    path.ancestors().take_while(move |ancestor| {
        if done {
            return false;
        }
        if top.as_deref() == Some(*ancestor) {
            done = true;
        }
        true
    })
}

/// The folder `path` is in, short of going above a server's or phone's top.
pub fn parent(path: &Path) -> Option<&Path> {
    ancestors(path).nth(1)
}

/// Whether a folder can be read without waiting.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Listing {
    /// Read it: it's local, or listed a moment ago.
    Ready,
    /// Being connected to or listed; it'll repaint once it is.
    Loading,
    /// It couldn't be listed, for this reason. It stays so until [`retry`].
    Failed(String),
    /// It's this folder: a home folder (`~`) found once connected.
    Moved(PathBuf),
}

/// Sets up for servers: call once, on the UI's thread, before anything
/// touches a server path.
pub fn init(ctx: &egui::Context) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = CTX.set(ctx.clone());
        disk::sftp::never_connect_on_this_thread();
        prompts::init(ctx);
    }
    #[cfg(target_arch = "wasm32")]
    let _ = ctx;
}

/// Asks the user what ssh wants to know, if it's waiting on them.
pub fn prompts_ui(ctx: &egui::Context) {
    #[cfg(not(target_arch = "wasm32"))]
    prompts::ui(ctx);
    notices_ui(ctx);
}

/// Whether `dir` can be read without waiting; if not, it's fetched.
pub fn listing(dir: &Path) -> Listing {
    #[cfg(not(target_arch = "wasm32"))]
    return fetch::listing(dir);
    #[cfg(target_arch = "wasm32")]
    {
        let _ = dir;
        Listing::Ready
    }
}

/// Forgets what's known of `dir`, and that it failed, so it's fetched again.
pub fn retry(dir: &Path) {
    #[cfg(not(target_arch = "wasm32"))]
    fetch::retry(dir);
    #[cfg(target_arch = "wasm32")]
    let _ = dir;
}

/// Forgets what's known of the folders in `root`, and that they failed, so
/// they're fetched again: a phone's, as it comes and goes.
pub fn retry_within(root: &Path) {
    #[cfg(not(target_arch = "wasm32"))]
    fetch::retry_within(root);
    #[cfg(target_arch = "wasm32")]
    let _ = root;
}

/// Whether the computer `path` is on is being connected to.
pub fn connecting(path: &Path) -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    return disk::sftp::status(path) == disk::sftp::Status::Connecting;
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        false
    }
}

#[cfg(not(target_arch = "wasm32"))]
static CTX: std::sync::OnceLock<egui::Context> = std::sync::OnceLock::new();

#[cfg(not(target_arch = "wasm32"))]
mod fetch {
    use std::{
        collections::{HashMap, HashSet},
        path::{Path, PathBuf},
        sync::{LazyLock, Mutex, MutexGuard, PoisonError},
    };

    use super::{CTX, Listing, is_remote};

    #[derive(Default)]
    struct Fetches {
        running: HashSet<PathBuf>,
        failed: HashMap<PathBuf, String>,
        moved: HashMap<PathBuf, PathBuf>,
    }

    static FETCHES: LazyLock<Mutex<Fetches>> = LazyLock::new(Mutex::default);

    fn fetches() -> MutexGuard<'static, Fetches> {
        FETCHES.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn listing(dir: &Path) -> Listing {
        if !is_remote(dir) {
            return Listing::Ready;
        }
        let mut state = fetches();
        if let Some(to) = state.moved.get(dir) {
            return Listing::Moved(to.clone());
        }
        if let Some(why) = state.failed.get(dir) {
            return Listing::Failed(why.clone());
        }
        if state.running.contains(dir) {
            return Listing::Loading;
        }
        if disk::sftp::is_listed(dir) {
            return Listing::Ready;
        }
        state.running.insert(dir.to_path_buf());
        drop(state);
        let dir = dir.to_path_buf();
        crate::background::spawn("list a server's folder", move || {
            let result = fetch(&dir);
            let mut fetches = fetches();
            fetches.running.remove(&dir);
            match result {
                Ok(Some(to)) => {
                    fetches.moved.insert(dir, to);
                }
                Ok(None) => {}
                Err(why) => {
                    fetches.failed.insert(dir, why);
                }
            }
            drop(fetches);
            if let Some(ctx) = CTX.get() {
                ctx.request_repaint();
            }
        });
        Listing::Loading
    }

    /// Connects and lists `dir`; if it's in a home folder, says where that is.
    fn fetch(dir: &Path) -> Result<Option<PathBuf>, String> {
        disk::sftp::connect(dir).map_err(|err| err.to_string())?;
        let location = disk::sftp::parse(dir).ok_or("not a server's path")?;
        super::remember(&location.authority);
        let in_home = location
            .path
            .strip_prefix("/~")
            .filter(|rest| rest.is_empty() || rest.starts_with('/'));
        let (target, moved) = match in_home {
            Some(rest) => {
                let home = disk::sftp::home(dir).map_err(|err| err.to_string())?;
                let target = PathBuf::from(format!("{}{rest}", home.display()));
                (target.clone(), Some(target))
            }
            None => (dir.to_path_buf(), None),
        };
        disk::read_dir(&target).map_err(|err| {
            if crate::phones::is_phone(dir) {
                "the phone isn't connected".to_owned()
            } else {
                err.to_string()
            }
        })?;
        Ok(moved)
    }

    pub fn retry_within(root: &Path) {
        let mut fetches = fetches();
        fetches.failed.retain(|dir, _| !dir.starts_with(root));
        drop(fetches);
        disk::sftp::forget(root);
    }

    pub fn retry(dir: &Path) {
        let mut fetches = fetches();
        fetches.failed.remove(dir);
        fetches.moved.remove(dir);
        drop(fetches);
        disk::sftp::forget(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestors_stop_at_the_server() {
        let path = Path::new("sftp://me@host/home/me");
        let all: Vec<_> = ancestors(path).collect();
        assert_eq!(
            all,
            [
                Path::new("sftp://me@host/home/me"),
                Path::new("sftp://me@host/home"),
                Path::new("sftp://me@host"),
            ]
        );
        assert_eq!(parent(Path::new("sftp://me@host")), None);
        assert_eq!(parent(Path::new("/a/b")), Some(Path::new("/a")));
        assert_eq!(ancestors(Path::new("/a")).count(), 2);
    }

    #[test]
    fn typing_a_server_alone_means_its_home() {
        assert_eq!(typed("sftp://host".into()), PathBuf::from("sftp://host/~"));
        assert_eq!(typed("sftp://host/".into()), PathBuf::from("sftp://host/"));
        assert_eq!(typed("/tmp".into()), PathBuf::from("/tmp"));
    }
}
