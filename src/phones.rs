//! Phones paired with KDE Connect, listed with the drives. KDE Connect mounts
//! a phone's storage on request, with sshfs, so once it's connected its
//! folders are ordinary paths; the keys and the phone's SFTP server stay its
//! business.
//!
//! The UI's thread never talks to KDE Connect: background threads do, and it
//! reads what they found.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use egui::Context;
use egui_phosphor::regular as icon;

use crate::places::Place;

/// A reachable, paired phone.
#[derive(Clone, Debug, PartialEq)]
pub struct Phone {
    pub id: String,
    pub name: String,
    /// Its storage folders with what they're called, by path; none until
    /// it's mounted.
    pub folders: Vec<(PathBuf, String)>,
}

/// What's going on with a phone that isn't mounted yet.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Idle,
    Mounting,
    Failed(String),
}

/// A phone to offer connecting to.
pub struct Unmounted {
    pub id: String,
    pub name: String,
    pub status: Status,
}

/// The top of each storage folder mounted, with what to call it. Global as
/// paths are shown all over, far from the sidebar.
static FOLDERS: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());

fn folders() -> MutexGuard<'static, Vec<(PathBuf, String)>> {
    FOLDERS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What to call `path`, if it's the top of a phone's storage.
pub fn name_of(path: &Path) -> Option<String> {
    let folders = folders();
    let found = folders.iter().find(|(root, _)| root == path);
    found.map(|(_, name)| name.clone())
}

/// The top of the phone's storage that `path` is in, if it's in one.
pub fn root_of(path: &Path) -> Option<PathBuf> {
    let folders = folders();
    let found = folders.iter().find(|(root, _)| path.starts_with(root));
    found.map(|(root, _)| root.clone())
}

#[derive(Default)]
struct State {
    phones: Vec<Phone>,
    mounting: Vec<String>,
    failed: Vec<(String, String)>,
    /// A folder just mounted to show, and whether in a new tab.
    open: Option<(PathBuf, bool)>,
    /// Folders that went away, which tabs must leave without looking at them:
    /// the mount may be hanging on a phone that's gone.
    lost: Vec<PathBuf>,
}

pub struct Phones {
    state: Arc<Mutex<State>>,
    ctx: Context,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Puts in what KDE Connect says now, noting folders gone since last time.
#[cfg(target_os = "linux")]
fn store(state: &Mutex<State>, ctx: &Context, phones: Vec<Phone>) {
    let mut state = lock(state);
    if state.phones == phones {
        return;
    }
    let lost = lost_folders(&state.phones, &phones);
    state.lost.extend(lost);
    *folders() = phones
        .iter()
        .flat_map(|phone| {
            let named = |(path, folder): &(PathBuf, String)| (path.clone(), label(phone, folder));
            phone.folders.iter().map(named)
        })
        .collect();
    state.phones = phones;
    ctx.request_repaint();
}

#[cfg(target_os = "linux")]
fn lost_folders(before: &[Phone], now: &[Phone]) -> Vec<PathBuf> {
    let folders = |phones: &[Phone]| -> Vec<PathBuf> {
        phones
            .iter()
            .flat_map(|p| p.folders.iter().map(|(path, _)| path.clone()))
            .collect()
    };
    let now = folders(now);
    folders(before)
        .into_iter()
        .filter(|folder| !now.contains(folder))
        .collect()
}

/// A folder's row in the drives: a phone with just one goes by its own name.
fn label(phone: &Phone, folder: &str) -> String {
    if phone.folders.len() > 1 {
        format!("{}: {folder}", phone.name)
    } else {
        phone.name.clone()
    }
}

impl Phones {
    pub fn new(ctx: &Context) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        #[cfg(target_os = "linux")]
        {
            let shared = state.clone();
            let ctx = ctx.clone();
            let _ = std::thread::Builder::new()
                .name("phones".into())
                .spawn(move || kde::poll(&shared, &ctx));
        }
        Self {
            state,
            ctx: ctx.clone(),
        }
    }

    /// The folders of the phones that are mounted, as drives.
    pub fn places(&self) -> Vec<Place> {
        let state = lock(&self.state);
        let mut places = Vec::new();
        for phone in &state.phones {
            for (path, folder) in &phone.folders {
                places.push(Place {
                    label: label(phone, folder),
                    icon: icon::DEVICE_MOBILE,
                    path: path.clone(),
                    ejectable: true,
                    media: Some(0),
                    // sftp makes up its numbers: this phone says 1000 GB, all free.
                    space: None,
                });
            }
        }
        places
    }

    /// The phones to offer connecting to.
    pub fn unmounted(&self) -> Vec<Unmounted> {
        let state = lock(&self.state);
        state
            .phones
            .iter()
            .filter(|p| p.folders.is_empty())
            .map(|p| {
                let status = if state.mounting.contains(&p.id) {
                    Status::Mounting
                } else if let Some((_, why)) = state.failed.iter().find(|(id, _)| *id == p.id) {
                    Status::Failed(why.clone())
                } else {
                    Status::Idle
                };
                Unmounted {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    status,
                }
            })
            .collect()
    }

    /// Mounts a phone in the background, then has its first folder shown.
    pub fn mount(&self, id: &str, new_tab: bool) {
        let mut state = lock(&self.state);
        if state.mounting.iter().any(|m| m == id) {
            return;
        }
        state.mounting.push(id.to_owned());
        state.failed.retain(|(failed, _)| failed != id);
        drop(state);
        self.ctx.request_repaint();
        #[cfg(target_os = "linux")]
        {
            let (shared, ctx, id) = (self.state.clone(), self.ctx.clone(), id.to_owned());
            crate::background::spawn("mount a phone", move || {
                let result = kde::mount(&id);
                if let Ok(phones) = kde::list_now() {
                    store(&shared, &ctx, phones);
                }
                let mut state = lock(&shared);
                state.mounting.retain(|m| *m != id);
                let first = state
                    .phones
                    .iter()
                    .find(|p| p.id == id)
                    .and_then(|p| p.folders.first())
                    .map(|(path, _)| path.clone());
                match (result, first) {
                    (Ok(()), Some(folder)) => state.open = Some((folder, new_tab)),
                    (Ok(()), None) => state
                        .failed
                        .push((id, "it has no storage to show".to_owned())),
                    (Err(why), _) => state.failed.push((id, why)),
                }
                drop(state);
                ctx.request_repaint();
            });
        }
        #[cfg(not(target_os = "linux"))]
        let _ = new_tab;
    }

    /// A folder that's just been mounted, to show.
    pub fn take_open(&self) -> Option<(PathBuf, bool)> {
        lock(&self.state).open.take()
    }

    /// Folders that went away since last asked.
    pub fn take_lost(&self) -> Vec<PathBuf> {
        std::mem::take(&mut lock(&self.state).lost)
    }
}

/// Unmounts the phone that `path` is on; `None` if it's not on one.
#[cfg(target_os = "linux")]
pub use kde::eject;

#[cfg(target_os = "linux")]
mod kde {
    use std::{
        collections::HashMap,
        io::ErrorKind,
        path::{Path, PathBuf},
        process::Command,
        sync::Mutex,
        time::Duration,
    };

    use egui::Context;
    use zbus::{
        Connection, OwnedVariant, Proxy,
        proxy::{Builder, CacheProperties},
    };

    use super::{Phone, State, store};

    const SERVICE: &str = "org.kde.kdeconnect";
    const POLL: Duration = Duration::from_secs(1);
    /// Without a session bus, there's no use in asking often.
    const RETRY: Duration = Duration::from_secs(30);

    async fn proxy<'a>(
        connection: &Connection,
        path: String,
        interface: &'a str,
    ) -> zbus::Result<Proxy<'a>> {
        let builder: Builder<'a, Proxy<'a>> = Builder::new(connection);
        builder
            .destination(SERVICE)
            .path(path)
            .interface(interface)
            .cache_properties(CacheProperties::No)
            .build()
            .await
    }

    async fn sftp<'a>(connection: &Connection, id: &str) -> zbus::Result<Proxy<'a>> {
        proxy(
            connection,
            format!("/modules/kdeconnect/devices/{id}/sftp"),
            "org.kde.kdeconnect.device.sftp",
        )
        .await
    }

    /// The phones to show. Asks whether KDE Connect is running first, as a
    /// call to it would start it, and using this app shouldn't do that.
    async fn list(connection: &Connection) -> zbus::Result<Vec<Phone>> {
        let bus = Proxy::new(
            connection,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await?;
        let running: bool = bus.call("NameHasOwner", &(SERVICE,)).await?;
        if !running {
            return Ok(Vec::new());
        }
        let daemon = proxy(
            connection,
            "/modules/kdeconnect".to_owned(),
            "org.kde.kdeconnect.daemon",
        )
        .await?;
        let names: HashMap<String, String> = daemon.call("deviceNames", &(true, true)).await?;
        let mut phones = Vec::new();
        for (id, name) in names {
            // Fails if the phone or this computer has the plugin turned off.
            if let Ok(folders) = folders(connection, &id, &name).await {
                phones.push(Phone { id, name, folders });
            }
        }
        phones.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(phones)
    }

    async fn folders(
        connection: &Connection,
        id: &str,
        name: &str,
    ) -> zbus::Result<Vec<(PathBuf, String)>> {
        let sftp = sftp(connection, id).await?;
        let mounted: bool = sftp.call("isMounted", &()).await?;
        if !mounted {
            return Ok(Vec::new());
        }
        let found: HashMap<String, OwnedVariant> = sftp.call("getDirectories", &()).await?;
        let mut folders: Vec<(PathBuf, String)> = found
            .into_iter()
            .filter_map(|(path, label)| Some((PathBuf::from(path), String::try_from(label).ok()?)))
            .collect();
        folders.sort();
        if folders.is_empty() {
            let root: String = sftp.call("mountPoint", &()).await?;
            folders.push((PathBuf::from(root), name.to_owned()));
        }
        Ok(folders)
    }

    pub fn poll(state: &Mutex<State>, ctx: &Context) {
        let mut connection = None;
        loop {
            if connection.is_none() {
                connection = zbus::block_on(Connection::session()).ok();
            }
            let Some(session) = &connection else {
                std::thread::sleep(RETRY);
                continue;
            };
            match zbus::block_on(list(session)) {
                Ok(phones) => store(state, ctx, phones),
                // Connects afresh next time; KDE Connect may have quit.
                Err(_) => {
                    store(state, ctx, Vec::new());
                    connection = None;
                }
            }
            std::thread::sleep(POLL);
        }
    }

    pub fn list_now() -> zbus::Result<Vec<Phone>> {
        zbus::block_on(async { list(&Connection::session().await?).await })
    }

    pub fn mount(id: &str) -> Result<(), String> {
        zbus::block_on(async {
            let connection = Connection::session().await?;
            let sftp = sftp(&connection, id).await?;
            let root: String = sftp.call("mountPoint", &()).await?;
            clear_dead_mount(Path::new(&root));
            let mounted: bool = sftp.call("mountAndWait", &()).await?;
            if mounted {
                return Ok(Ok(()));
            }
            // Only meant when mounting failed: it stays set after a success.
            let why: String = sftp.call("getMountError", &()).await?;
            Ok(Err(if why.is_empty() {
                "the phone didn't answer".to_owned()
            } else {
                why
            }))
        })
        .unwrap_or_else(|err: zbus::Error| Err(err.to_string()))
    }

    /// KDE Connect's own `unmount`, which is also how it lets go of a phone,
    /// kills sshfs and leaves the mount behind, dead; it can't mount over
    /// that until the mount is gone.
    fn clear_dead_mount(root: &Path) {
        let dead = std::fs::metadata(root).is_err_and(|err| err.kind() == ErrorKind::NotConnected);
        if dead {
            let _ = unmount(root);
        }
    }

    /// Unmounts cleanly, unlike KDE Connect's `unmount`; a mount in use is
    /// refused rather than cut off.
    fn unmount(root: &Path) -> Result<(), String> {
        let run = |program| Command::new(program).arg("-u").arg(root).output();
        let output = match run("fusermount3") {
            Err(err) if err.kind() == ErrorKind::NotFound => run("fusermount"),
            output => output,
        }
        .map_err(|err| format!("couldn't run fusermount: {err}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(unmount_error(&String::from_utf8_lossy(&output.stderr)))
        }
    }

    /// What went wrong, in words for the user: being in use is the usual
    /// reason, and fusermount doesn't say by what.
    fn unmount_error(stderr: &str) -> String {
        if stderr.contains("busy") {
            "the phone is in use: a program or terminal has one of its folders open".to_owned()
        } else {
            stderr.trim().to_owned()
        }
    }

    #[cfg(test)]
    #[test]
    fn a_busy_mount_is_explained() {
        let busy = "fusermount3: failed to unmount /run/user/1000/x: Device or resource busy\n";
        assert!(unmount_error(busy).starts_with("the phone is in use"));
        assert_eq!(unmount_error("fusermount3: nope\n"), "fusermount3: nope");
    }

    pub fn eject(path: &Path) -> Option<Result<(), String>> {
        let root = zbus::block_on(async {
            let connection = Connection::session().await?;
            for phone in list(&connection).await? {
                let root: String = sftp(&connection, &phone.id)
                    .await?
                    .call("mountPoint", &())
                    .await?;
                if path.starts_with(&root) {
                    return Ok::<_, zbus::Error>(Some(root));
                }
            }
            Ok(None)
        })
        .ok()??;
        Some(unmount(Path::new(&root)))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn phone(folders: &[&str]) -> Phone {
        Phone {
            id: "id".to_owned(),
            name: "Pixel".to_owned(),
            folders: folders
                .iter()
                .map(|f| (PathBuf::from(format!("/m/{f}")), (*f).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn a_phone_with_one_folder_goes_by_its_name() {
        assert_eq!(label(&phone(&["Internal"]), "Internal"), "Pixel");
        assert_eq!(
            label(&phone(&["Internal", "SD card"]), "SD card"),
            "Pixel: SD card"
        );
    }

    /// Paths stop at the phone's storage, which goes by the phone's name, and
    /// nothing else is affected, not even a folder whose name merely starts
    /// the same.
    #[test]
    fn the_top_of_a_phones_storage_is_a_root() {
        let top = PathBuf::from("/run/phone-test/storage/0");
        folders().push((top.clone(), "Pixel".to_owned()));
        let inside = top.join("DCIM");

        assert_eq!(crate::fs::location_name(&top), "Pixel");
        assert_eq!(crate::fs::display_name(&top), "0");
        assert_eq!(crate::fs::location_name(&inside), "DCIM");
        assert_eq!(crate::remote::parent(&inside), Some(top.as_path()));
        assert_eq!(crate::remote::parent(&top), None);
        assert_eq!(crate::remote::ancestors(&inside).count(), 2);

        let beside = Path::new("/run/phone-test/storage/01");
        assert_eq!(root_of(beside), None);
        assert_eq!(crate::remote::parent(beside), beside.parent());
    }

    #[test]
    fn folders_gone_are_noticed() {
        let both = [phone(&["a", "b"])];
        let one = [phone(&["b"])];
        assert_eq!(lost_folders(&both, &one), [PathBuf::from("/m/a")]);
        assert!(lost_folders(&one, &both).is_empty());
        assert_eq!(lost_folders(&one, &[]), [PathBuf::from("/m/b")]);
    }
}
