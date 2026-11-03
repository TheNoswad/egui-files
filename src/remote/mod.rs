//! Folders on servers (`sftp://…` paths; see `disk::sftp`): listing them
//! off the UI's thread, and asking the user what ssh wants to know.
//!
//! The UI's thread never connects, nor waits on a server for a listing:
//! [`listing`] says whether a folder's ready to read, and if not, fetches it
//! in the background and repaints once it's in. A path whose first part is
//! `~` (`sftp://host/~/notes`) means the user's home folder there, which is
//! only known once connected.

#[cfg(not(target_arch = "wasm32"))]
mod prompts;

use std::path::{Path, PathBuf};

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

/// `path` and the folders it's in, up to the top of its drive or server.
pub fn ancestors(path: &Path) -> impl Iterator<Item = &Path> {
    let top = root(path);
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

/// The folder `path` is in, short of going above a server's top.
pub fn parent(path: &Path) -> Option<&Path> {
    ancestors(path).nth(1)
}

/// Whether a folder can be read without waiting.
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
    #[cfg(target_arch = "wasm32")]
    let _ = ctx;
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
        disk::read_dir(&target).map_err(|err| err.to_string())?;
        Ok(moved)
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
