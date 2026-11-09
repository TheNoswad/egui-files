//! Opening a server's files in other apps, which can't read them there: a
//! copy is fetched into the cache folder and opened instead. While the app
//! runs, whatever saves the copy is put back on the server.
//!
//! Each copy's folder is named for its server and path, so opening a file
//! again reuses its copy, fetched afresh unless it has changes not yet put
//! back.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
    time::{Duration, SystemTime},
};

/// How often copies are looked at for changes to put back.
const CHECK_EVERY: Duration = Duration::from_secs(2);

struct Copy {
    local: PathBuf,
    remote: PathBuf,
    /// When the copy was last changed, as fetched or put back.
    synced: Option<SystemTime>,
}

static COPIES: Mutex<Vec<Copy>> = Mutex::new(Vec::new());
static WATCHING: std::sync::Once = std::sync::Once::new();

fn copies() -> std::sync::MutexGuard<'static, Vec<Copy>> {
    COPIES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The Drive's copies, within [`cache_base`]: readable by anyone who gets at
/// this computer's files, unlike the Drive, so gone on signing out.
const PROTON_FOLDER: &str = "proton-drive";

fn cache_base() -> io::Result<PathBuf> {
    Ok(directories::BaseDirs::new()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no cache folder"))?
        .cache_dir()
        .join("egui-files"))
}

/// Forgets and removes every copy of the Drive's files.
#[cfg(not(target_os = "android"))]
pub fn forget_proton() -> io::Result<()> {
    copies().retain(|copy| !disk::proton::is_proton(&copy.remote));
    match disk::remove_dir_all(cache_base()?.join(PROTON_FOLDER)) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// Where the copy of `remote` goes: under the cache folder, by server and
/// path, with the file's own name so apps know what it is.
fn local_path(remote: &Path) -> io::Result<PathBuf> {
    let base = cache_base()?;
    if let Some(parts) = disk::proton::parts(remote) {
        let mut path = base.join(PROTON_FOLDER);
        path.extend(parts);
        return Ok(path);
    }
    let location = disk::sftp::parse(remote)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a server's path"))?;
    let base = base.join("servers");
    // Characters some systems don't allow in names, out of the server's.
    let server: String = location
        .authority
        .chars()
        .map(|c| if "<>:\"|?*".contains(c) { '_' } else { c })
        .collect();
    let mut path = base.join(server);
    for part in location.path.split('/').filter(|p| !p.is_empty()) {
        path.push(part);
    }
    Ok(path)
}

fn modified(path: &Path) -> Option<SystemTime> {
    disk::metadata(path).and_then(|m| m.modified()).ok()
}

/// Fetches `remote` into its copy, unless the copy has changes still to
/// put back; returns the copy.
fn fetch(remote: &Path) -> io::Result<PathBuf> {
    let local = local_path(remote)?;
    let unsaved = copies()
        .iter()
        .any(|c| c.local == local && c.synced != modified(&local));
    if !unsaved {
        if let Some(dir) = local.parent() {
            disk::create_dir_all(dir)?;
        }
        disk::copy(remote, &local)?;
        // As on the server, so the app shows when it was changed.
        if let Ok(time) = disk::metadata(remote).and_then(|m| m.modified()) {
            let _ = disk::set_file_mtime(&local, time);
        }
    }
    let synced = modified(&local);
    let mut copies = copies();
    copies.retain(|c| c.local != local);
    copies.push(Copy {
        local: local.clone(),
        remote: remote.to_path_buf(),
        synced: if unsaved { None } else { synced },
    });
    drop(copies);
    WATCHING.call_once(|| crate::background::spawn("put back servers' files", watch));
    Ok(local)
}

/// Puts back copies changed since; forever, a few seconds apart.
fn watch() {
    loop {
        std::thread::sleep(CHECK_EVERY);
        let changed: Vec<(PathBuf, PathBuf, Option<SystemTime>)> = copies()
            .iter()
            .filter_map(|c| {
                let now = modified(&c.local)?;
                (Some(now) != c.synced).then(|| (c.local.clone(), c.remote.clone(), Some(now)))
            })
            .collect();
        for (local, remote, now) in changed {
            let result = put_back(&local, &remote);
            let mut copies = copies();
            if let Some(copy) = copies.iter_mut().find(|c| c.local == local) {
                // Tried once per change: a failure waits for the next save.
                copy.synced = now;
            }
            drop(copies);
            if let Err(err) = result {
                super::notify(format!(
                    "Couldn't save {} back to the server: {err}. Your changes are in {}.",
                    crate::fs::display_name(&remote),
                    local.display()
                ));
            }
        }
    }
}

/// Puts the copy at `local` back at `remote`: written beside it, then
/// renamed over it, so a dropped connection doesn't leave half a file.
fn put_back(local: &Path, remote: &Path) -> io::Result<()> {
    // The Drive never shows half a file, as each save is a new revision.
    if disk::proton::is_proton(remote) {
        return disk::copy(local, remote).map(drop);
    }
    let name = crate::fs::display_name(remote);
    let temp = remote.with_file_name(format!(".{name}.egui-files-{}.tmp", disk::process_id()));
    let result = disk::copy(local, &temp).and_then(|_| disk::rename(&temp, remote));
    if result.is_err() {
        let _ = disk::remove_file(&temp);
    }
    result
}

/// Opens a server's file in its app, by way of a copy.
pub fn open(remote: &Path) {
    let remote = remote.to_path_buf();
    crate::background::spawn("open a server's file", move || {
        let result = fetch(&remote).and_then(|local| crate::fs::open(&local));
        if let Err(err) = result {
            super::notify(format!(
                "Couldn't open {}: {err}",
                crate::fs::display_name(&remote)
            ));
        }
    });
}

/// The most a drag out of a server fetches, as it waits for it.
pub const MAX_DRAG_OUT: u64 = 16 << 20;

/// Copies of `paths`, with those on servers fetched now, waiting for them:
/// for dragging out, where the other app needs files at once.
pub fn fetch_now(paths: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    let size: u64 = paths
        .iter()
        .filter(|p| super::is_remote(p))
        .map(|p| disk::metadata(p).map(|m| if m.is_dir() { u64::MAX } else { m.len() }))
        .sum::<io::Result<u64>>()?;
    if size > MAX_DRAG_OUT {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "only files of up to 16 MB in all can be dragged out of a server; copy them here first",
        ));
    }
    paths
        .iter()
        .map(|p| {
            if super::is_remote(p) {
                fetch(p)
            } else {
                Ok(p.clone())
            }
        })
        .collect()
}

/// Fetches copies of `paths` that are on servers, then calls `then` with
/// what to open in their place, from another thread.
pub fn with_copies(paths: Vec<PathBuf>, then: impl FnOnce(Vec<PathBuf>) + Send + 'static) {
    crate::background::spawn("fetch servers' files", move || {
        let mut locals = Vec::with_capacity(paths.len());
        for path in paths {
            if !super::is_remote(&path) {
                locals.push(path);
                continue;
            }
            match fetch(&path) {
                Ok(local) => locals.push(local),
                Err(err) => super::notify(format!(
                    "Couldn't open {}: {err}",
                    crate::fs::display_name(&path)
                )),
            }
        }
        if !locals.is_empty() {
            then(locals);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_go_by_server_and_path() {
        let local = local_path(Path::new("sftp://me@host:22/srv/a b/notes.txt")).unwrap();
        assert!(local.ends_with(Path::new("servers/me@host_22/srv/a b/notes.txt")));
        assert!(local_path(Path::new("/tmp/x")).is_err());
        let local = local_path(Path::new("proton://drive/Docs/a.txt")).unwrap();
        assert!(local.ends_with(Path::new("proton-drive/Docs/a.txt")));
    }

    /// Against a server: see disk/src/sftp/tests.rs for
    /// `EGUI_FILES_SFTP_TEST_CONFIG`. Without it, this does nothing.
    #[test]
    fn changes_to_a_copy_go_back_to_the_server() {
        let Some(config) = std::env::var_os("EGUI_FILES_SFTP_TEST_CONFIG") else {
            return;
        };
        disk::sftp::set_extra_ssh_args([
            std::ffi::OsString::from("-F"),
            config,
            "-o".into(),
            "ControlPath=none".into(),
        ]);
        let server = Path::new("sftp://sftp-test");
        disk::sftp::connect(server).unwrap();
        let remote = disk::sftp::home(server)
            .unwrap()
            .join(format!("copies-{}.txt", std::process::id()));
        disk::write(&remote, "first").unwrap();

        let (send, opened) = std::sync::mpsc::channel();
        with_copies(vec![remote.clone()], move |locals| {
            send.send(locals).unwrap();
        });
        let locals = opened.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(disk::read_to_string(&locals[0]).unwrap(), "first");
        // As an app saves it, a moment later.
        std::thread::sleep(Duration::from_millis(1100));
        disk::write(&locals[0], "second").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            disk::sftp::forget(&remote);
            if disk::read_to_string(&remote).unwrap() == "second" {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "never put back");
            std::thread::sleep(Duration::from_millis(200));
        }
        disk::remove_file(&remote).unwrap();
        disk::remove_file(&locals[0]).unwrap();
    }
}
