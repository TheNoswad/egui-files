//! Phones paired with KDE Connect, listed with the drives.
//!
//! KDE Connect mounts a phone's storage on request, with sshfs, keeping the
//! keys and the phone's SFTP server its business. Reading that mount can wait
//! for as long as the phone's away, which the UI's thread must never do. So
//! the app doesn't read it as a folder: it reaches it as a server's
//! (`sftp://kdeconnect-<id>/…`), through an `sftp-server` running here (see
//! `disk::sftp::set_local_servers`). What waits on the phone is that process
//! and background threads; the UI's thread is only ever told what's known.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use egui::Context;
use egui_phosphor::regular as icon;

use crate::places::Place;

/// What a phone's paths give as the computer they're on, before its id.
const AUTHORITY_PREFIX: &str = "kdeconnect-";

/// A reachable, paired phone.
#[derive(Clone, Debug, PartialEq)]
pub struct Phone {
    pub id: String,
    pub name: String,
    /// Where it's mounted, or will be: not to be read on the UI's thread.
    pub root: PathBuf,
    /// Its storage folders (as `sftp://` paths) with what they're called;
    /// none until it's mounted.
    pub folders: Vec<(PathBuf, String)>,
    /// sshfs is gone but its mount was left behind, which is how KDE Connect
    /// lets go of a phone it lost. An eject leaves nothing.
    pub dead_mount: bool,
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

/// The top of each storage folder seen, with what to call it. Global as paths
/// are shown all over, far from the sidebar; kept when a phone goes, as tabs
/// stay on its folders to pick up where they were once it's back.
static FOLDERS: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());

fn folders() -> MutexGuard<'static, Vec<(PathBuf, String)>> {
    FOLDERS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where each phone is mounted, and the computer's name its paths go by.
#[cfg(not(target_arch = "wasm32"))]
static MOUNTS: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());

/// `path` as the app must reach it, if it's in a phone's mount: a tab saved
/// on the mount itself, a link, or a terminal that's there would otherwise
/// have the UI's thread read it.
pub fn reached(path: &Path) -> Option<PathBuf> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mounts = MOUNTS.lock().unwrap_or_else(PoisonError::into_inner);
        let (_, authority) = mounts.iter().find(|(root, _)| path.starts_with(root))?;
        disk::sftp::url(authority, path.to_str()?).into()
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        None
    }
}

/// The file in the phone's mount that `path` means, for programs of other
/// people's that read it themselves and can't reach `sftp://kdeconnect-…`:
/// they run off the UI's thread, and it's fine for them to wait on the phone.
pub fn mount_path(path: &Path) -> Option<PathBuf> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        is_phone(path).then(|| disk::sftp::parse(path).map(|location| location.path.into()))?
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        None
    }
}

/// Whether `path` is on a phone.
pub fn is_phone(path: &Path) -> bool {
    path.to_str()
        .and_then(|path| path.strip_prefix("sftp://"))
        .is_some_and(|rest| rest.starts_with(AUTHORITY_PREFIX))
}

/// Whether this computer's name, as a path gives it, is a phone's.
pub fn is_phone_authority(authority: &str) -> bool {
    authority.starts_with(AUTHORITY_PREFIX)
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
    /// Folders that went away and came back, for tabs to look at again.
    changed: Vec<PathBuf>,
    /// Phones lost while mounted, by id, to mount again once they're back.
    /// One the user ejected isn't among them.
    dropped: Vec<String>,
    /// How mounting each dropped phone again has gone so far.
    #[cfg(target_os = "linux")]
    remounts: std::collections::HashMap<String, Remount>,
}

/// Tries at mounting a dropped phone again. Each one that fails is a
/// notification from KDE Connect, so they're spaced out, then given up on:
/// the phone shows why, and a click tries again.
#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq)]
struct Remount {
    failed: usize,
    /// When to try next; `None` once given up on.
    next: Option<std::time::Instant>,
}

/// How long after each failed try to mount a dropped phone again the next
/// is; after the last, it's given up on.
#[cfg(target_os = "linux")]
const REMOUNT_AFTER: [std::time::Duration; 3] = [
    std::time::Duration::from_secs(5),
    std::time::Duration::from_secs(30),
    std::time::Duration::from_secs(120),
];

/// Whether it's time to try mounting the dropped phone `id` again.
#[cfg(target_os = "linux")]
fn remount_due(state: &State, id: &str, now: std::time::Instant) -> bool {
    state
        .remounts
        .get(id)
        .is_none_or(|r| r.next.is_some_and(|next| now >= next))
}

/// Notes how mounting `id` went, if it's one that dropped.
#[cfg(target_os = "linux")]
fn note_remount(state: &mut State, id: &str, mounted: bool, now: std::time::Instant) {
    if mounted {
        state.remounts.remove(id);
        return;
    }
    if !state.dropped.iter().any(|d| d == id) {
        return;
    }
    let remount = state.remounts.entry(id.to_owned()).or_insert(Remount {
        failed: 0,
        next: None,
    });
    remount.next = REMOUNT_AFTER.get(remount.failed).map(|after| now + *after);
    remount.failed += 1;
}

/// A phone that left and is back has reconnected, so gets its tries afresh.
#[cfg(target_os = "linux")]
fn forget_remounts_of_unlisted(state: &mut State) {
    let phones = &state.phones;
    state
        .remounts
        .retain(|id, _| phones.iter().any(|p| p.id == *id));
}

pub struct Phones {
    state: Arc<Mutex<State>>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    ctx: Context,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Puts in what KDE Connect says now, noting folders gone or new since last
/// time and phones that vanished.
#[cfg(target_os = "linux")]
fn store(state: &Mutex<State>, ctx: &Context, phones: Vec<Phone>) {
    let mut state = lock(state);
    if state.phones == phones {
        return;
    }
    let gone = difference(&state.phones, &phones);
    let new = difference(&phones, &state.phones);
    for id in vanished(&state.phones, &phones) {
        note_dropped(&mut state, id);
    }
    let dead = phones.iter().filter(|p| p.dead_mount);
    for id in dead.map(|p| p.id.clone()).collect::<Vec<_>>() {
        note_dropped(&mut state, id);
    }
    state.changed.extend(gone.iter().chain(&new).cloned());
    {
        let mut known = folders();
        for phone in &phones {
            for (path, folder) in &phone.folders {
                known.retain(|(root, _)| root != path);
                known.push((path.clone(), label(phone, folder)));
            }
        }
    }
    {
        let mut mounts = MOUNTS.lock().unwrap_or_else(PoisonError::into_inner);
        for phone in &phones {
            mounts.retain(|(root, _)| *root != phone.root);
            mounts.push((phone.root.clone(), kde::authority(&phone.id)));
        }
    }
    state.phones = phones;
    drop(state);
    ctx.request_repaint();
    // Waits for what's using the connection, which may be waiting on the phone.
    if !gone.is_empty() {
        crate::background::spawn("let go of a phone", move || {
            for folder in gone {
                disk::sftp::disconnect(&folder);
            }
        });
    }
}

#[cfg(target_os = "linux")]
fn note_dropped(state: &mut State, id: String) {
    if !state.dropped.contains(&id) {
        state.dropped.push(id);
    }
}

/// The folders of `of` that `other` doesn't have.
#[cfg(target_os = "linux")]
fn difference(of: &[Phone], other: &[Phone]) -> Vec<PathBuf> {
    let folders = |phones: &[Phone]| -> Vec<PathBuf> {
        phones
            .iter()
            .flat_map(|p| p.folders.iter().map(|(path, _)| path.clone()))
            .collect()
    };
    let other = folders(other);
    folders(of)
        .into_iter()
        .filter(|folder| !other.contains(folder))
        .collect()
}

/// The phones that were mounted and aren't even listed now.
#[cfg(target_os = "linux")]
fn vanished(before: &[Phone], now: &[Phone]) -> Vec<String> {
    before
        .iter()
        .filter(|p| !p.folders.is_empty() && !now.iter().any(|n| n.id == p.id))
        .map(|p| p.id.clone())
        .collect()
}

/// Mounts a phone in the background, then has its first folder shown, if
/// `open` says so and whether in a new tab.
#[cfg(target_os = "linux")]
fn start_mount(shared: &Arc<Mutex<State>>, ctx: &Context, id: &str, open: Option<bool>) {
    let mut state = lock(shared);
    if state.mounting.iter().any(|m| m == id) {
        return;
    }
    state.mounting.push(id.to_owned());
    state.failed.retain(|(failed, _)| failed != id);
    drop(state);
    ctx.request_repaint();
    let (shared, ctx, id) = (shared.clone(), ctx.clone(), id.to_owned());
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
        let mounted = result.is_ok() && first.is_some();
        note_remount(&mut state, &id, mounted, std::time::Instant::now());
        match (result, first) {
            (Ok(()), Some(folder)) => {
                state.dropped.retain(|dropped| *dropped != id);
                if let Some(new_tab) = open {
                    state.open = Some((folder, new_tab));
                }
            }
            (Ok(()), None) => state
                .failed
                .push((id, "it has no storage to show".to_owned())),
            (Err(why), _) => state.failed.push((id, why)),
        }
        drop(state);
        ctx.request_repaint();
    });
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
            kde::set_up();
            let (shared, repaint) = (state.clone(), ctx.clone());
            let _ = std::thread::Builder::new()
                .name("phones".into())
                .spawn(move || kde::poll(&shared, &repaint));
            let (shared, repaint) = (state.clone(), ctx.clone());
            let _ = std::thread::Builder::new()
                .name("phone watchdog".into())
                .spawn(move || kde::watch(&shared, &repaint));
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
                } else if state.dropped.contains(&p.id) {
                    // Between tries to mount it again.
                    Status::Mounting
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
        #[cfg(target_os = "linux")]
        start_mount(&self.state, &self.ctx, id, Some(new_tab));
        #[cfg(not(target_os = "linux"))]
        let _ = (id, new_tab);
    }

    /// A folder that's just been mounted, to show.
    pub fn take_open(&self) -> Option<(PathBuf, bool)> {
        lock(&self.state).open.take()
    }

    /// Folders that went away or came back since last asked.
    pub fn take_changed(&self) -> Vec<PathBuf> {
        std::mem::take(&mut lock(&self.state).changed)
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
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering::Relaxed},
            mpsc,
        },
        time::{Duration, Instant},
    };

    use egui::Context;
    use zbus::{
        Connection, OwnedVariant, Proxy,
        proxy::{Builder, CacheProperties},
    };

    use super::{
        AUTHORITY_PREFIX, Phone, State, forget_remounts_of_unlisted, lock, note_dropped,
        remount_due, start_mount, store,
    };

    const SERVICE: &str = "org.kde.kdeconnect";
    const POLL: Duration = Duration::from_secs(1);
    /// Without a session bus, there's no use in asking often.
    const RETRY: Duration = Duration::from_secs(30);
    /// How often a mounted phone is checked on.
    const CHECK_EVERY: Duration = Duration::from_secs(5);
    /// How long a phone gets to answer before its connection is cut, so it
    /// shows as gone and is mounted again once it's back. Nothing on the UI's
    /// thread is waiting on this, so a phone that's asleep gets its time.
    const ANSWER_WITHIN: Duration = Duration::from_secs(10);
    /// How long KDE Connect gets to mount a phone. Its own limit is shorter.
    const MOUNT_WITHIN: Duration = Duration::from_secs(30);
    /// How long KDE Connect gets to notice a mount that was cut is over.
    const UNMOUNT_TRIES: u32 = 40;
    const UNMOUNT_TRY: Duration = Duration::from_millis(250);
    /// OpenSSH's SFTP server, where distributions put it.
    const SFTP_SERVERS: [&str; 4] = [
        "/usr/lib/ssh/sftp-server",
        "/usr/lib/openssh/sftp-server",
        "/usr/libexec/openssh/sftp-server",
        "/usr/libexec/sftp-server",
    ];

    fn sftp_server() -> Option<&'static str> {
        SFTP_SERVERS
            .into_iter()
            .find(|path| Path::new(path).exists())
    }

    /// Has phones' paths reach them through an `sftp-server` here, so nothing
    /// looks for them on the network, even before they're listed: a tab left
    /// on one last time asks at once.
    pub fn set_up() {
        disk::sftp::set_local_servers(AUTHORITY_PREFIX, sftp_server().unwrap_or("sftp-server"));
    }

    pub fn authority(id: &str) -> String {
        format!("{AUTHORITY_PREFIX}{id}")
    }

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
            if let Ok((root, folders, dead_mount)) = folders(connection, &id, &name).await {
                phones.push(Phone {
                    id,
                    name,
                    root,
                    folders,
                    dead_mount,
                });
            }
        }
        phones.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(phones)
    }

    fn mount_table() -> String {
        std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default()
    }

    /// Where a phone's mounted (or will be), its folders if it is, and whether
    /// a dead mount was left there. KDE Connect goes on saying a mount that was
    /// cut is there, until it notices sshfs ended, so the mount table has the
    /// last word.
    async fn folders(
        connection: &Connection,
        id: &str,
        name: &str,
    ) -> zbus::Result<(PathBuf, Vec<(PathBuf, String)>, bool)> {
        let sftp = sftp(connection, id).await?;
        let root: String = sftp.call("mountPoint", &()).await?;
        let mounted: bool = sftp.call("isMounted", &()).await?;
        let in_table = fuse_connection(&mount_table(), Path::new(&root)).is_some();
        if !mounted || !in_table {
            return Ok((PathBuf::from(root), Vec::new(), !mounted && in_table));
        }
        let found: HashMap<String, OwnedVariant> = sftp.call("getDirectories", &()).await?;
        let authority = authority(id);
        let mut folders: Vec<(PathBuf, String)> = found
            .into_iter()
            .filter_map(|(path, label)| {
                let label = String::try_from(label).ok()?;
                Some((disk::sftp::url(&authority, &path), label))
            })
            .collect();
        folders.sort();
        if folders.is_empty() {
            folders.push((disk::sftp::url(&authority, &root), name.to_owned()));
        }
        Ok((PathBuf::from(root), folders, false))
    }

    pub fn poll(state: &Arc<Mutex<State>>, ctx: &Context) {
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
            remount_dropped(state, ctx);
            std::thread::sleep(POLL);
        }
    }

    /// Mounts the phones that dropped again, now they're listed again.
    fn remount_dropped(state: &Arc<Mutex<State>>, ctx: &Context) {
        let mut locked = lock(state);
        let shared = &mut *locked;
        let phones = &shared.phones;
        shared
            .dropped
            .retain(|id| !phones.iter().any(|p| p.id == *id && !p.folders.is_empty()));
        forget_remounts_of_unlisted(shared);
        let now = Instant::now();
        let due: Vec<String> = shared
            .phones
            .iter()
            .filter(|p| shared.dropped.contains(&p.id) && p.folders.is_empty())
            .filter(|p| !shared.mounting.contains(&p.id))
            .filter(|p| remount_due(shared, &p.id, now))
            .map(|p| p.id.clone())
            .collect();
        drop(locked);
        for id in due {
            start_mount(state, ctx, &id, None);
        }
    }

    /// Notices a phone that's gone quiet, sooner than KDE Connect or sshfs
    /// would: its tabs say so instead of spinning, and it's mounted again as
    /// soon as it's back, which sshfs's own reconnecting doesn't show.
    pub fn watch(state: &Arc<Mutex<State>>, ctx: &Context) {
        loop {
            std::thread::sleep(CHECK_EVERY);
            let mounted: Vec<(String, PathBuf)> = lock(state)
                .phones
                .iter()
                .filter(|p| !p.folders.is_empty())
                .map(|p| (p.id.clone(), p.root.clone()))
                .collect();
            for (id, root) in mounted {
                if answers(&root) {
                    continue;
                }
                cut(&id, &root);
                note_dropped(&mut lock(state), id);
                if let Ok(phones) = list_now() {
                    store(state, ctx, phones);
                }
            }
        }
    }

    /// Whether the phone mounted at `root` answers soon, and sensibly. Asks
    /// after a file that isn't there, by a name not used before as sshfs
    /// remembers what it has seen and would answer for the phone. Nothing is
    /// written.
    fn answers(root: &Path) -> bool {
        static ASKED: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            ".egui-files-check-{}-{}",
            std::process::id(),
            ASKED.fetch_add(1, Relaxed)
        );
        let path = root.join(name);
        let (answered, answer) = mpsc::channel();
        let asked = std::thread::Builder::new()
            .name("phone check".into())
            .spawn(move || {
                let error = std::fs::symlink_metadata(path).err();
                let _ = answered.send(error.and_then(|err| err.raw_os_error()));
            });
        if asked.is_err() {
            return true;
        }
        match answer.recv_timeout(ANSWER_WITHIN) {
            Ok(error) => !error.is_some_and(means_cut_off),
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => true,
        }
    }

    /// Whether this error says the phone can't be reached, rather than
    /// anything about the file. sshfs goes on running when the phone's server
    /// restarts, as it does when the phone rejoins a network: it can't get
    /// back in with what it knows, and fails everything at once with `EIO`,
    /// for good. Only mounting afresh gets a phone that's back.
    fn means_cut_off(error: i32) -> bool {
        [
            libc::EIO,
            libc::ENOTCONN,
            libc::ENODEV,
            libc::ECONNABORTED,
            libc::ECONNRESET,
            libc::ECONNREFUSED,
            libc::ETIMEDOUT,
            libc::EHOSTUNREACH,
            libc::ENETUNREACH,
        ]
        .contains(&error)
    }

    /// Cuts a phone's connection: whatever's waiting on it gets an error now,
    /// and the mount goes. sshfs may be stuck too, so KDE Connect ends that.
    fn cut(id: &str, root: &Path) {
        if let Some(connection) = fuse_connection(&mount_table(), root) {
            let abort = format!("/sys/fs/fuse/connections/{connection}/abort");
            let _ = std::fs::write(abort, "1");
        }
        let _ = fusermount("-uz", root);
        let _ = zbus::block_on(async {
            let connection = Connection::session().await?;
            sftp(&connection, id)
                .await?
                .call_method("unmount", &())
                .await
        });
    }

    /// The number the kernel knows the FUSE mount at `root` by: the minor
    /// device number in the mount table, whose lines start
    /// `id parent major:minor root mount-point`.
    fn fuse_connection(mounts: &str, root: &Path) -> Option<String> {
        mounts.lines().rev().find_map(|line| {
            let mut fields = line.split(' ');
            let device = fields.nth(2)?;
            let mount = fields.nth(1)?;
            (Path::new(mount) == root).then(|| device.split_once(':').map(|(_, m)| m.to_owned()))?
        })
    }

    pub fn list_now() -> zbus::Result<Vec<Phone>> {
        zbus::block_on(async { list(&Connection::session().await?).await })
    }

    pub fn mount(id: &str) -> Result<(), String> {
        if sftp_server().is_none() {
            return Err("this needs OpenSSH's sftp-server, which wasn't found".to_owned());
        }
        zbus::block_on(async {
            let connection = Connection::session().await?;
            let sftp = sftp(&connection, id).await?;
            let root: String = sftp.call("mountPoint", &()).await?;
            let root = Path::new(&root);
            clear_dead_mount(root);
            // It'd say it's mounted, and do nothing, until told this one's over.
            let said: bool = sftp.call("isMounted", &()).await?;
            if said && fuse_connection(&mount_table(), root).is_none() {
                sftp.call_method("unmount", &()).await?;
                for _ in 0..UNMOUNT_TRIES {
                    let said: bool = sftp.call("isMounted", &()).await?;
                    if !said {
                        break;
                    }
                    std::thread::sleep(UNMOUNT_TRY);
                }
                clear_dead_mount(root);
            }
            // The table has the last word, as it may say so without mounting.
            let _ = mount_and_wait(id);
            if fuse_connection(&mount_table(), root).is_some() {
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

    /// Asks KDE Connect to mount a phone, for only so long: it's been seen never
    /// to answer, and a phone waiting on that wouldn't be tried again.
    fn mount_and_wait(id: &str) -> Result<bool, mpsc::RecvTimeoutError> {
        let (done, answer) = mpsc::channel();
        let id = id.to_owned();
        crate::background::spawn("ask KDE Connect to mount", move || {
            let mounted: zbus::Result<bool> = zbus::block_on(async {
                let connection = Connection::session().await?;
                sftp(&connection, &id)
                    .await?
                    .call("mountAndWait", &())
                    .await
            });
            let _ = done.send(mounted.unwrap_or(false));
        });
        answer.recv_timeout(MOUNT_WITHIN)
    }

    /// KDE Connect's own `unmount`, which is also how it lets go of a phone,
    /// kills sshfs and leaves the mount behind, dead; it can't mount over
    /// that until the mount is gone. A dead mount answers `ENOTCONN`, or
    /// `ECONNREFUSED` once its connection was aborted.
    fn clear_dead_mount(root: &Path) {
        let dead =
            std::fs::metadata(root).is_err_and(|err| err.raw_os_error().is_some_and(means_cut_off));
        if dead {
            let _ = fusermount("-uz", root);
        }
    }

    /// Runs fusermount: `-u` unmounts cleanly, unlike KDE Connect's `unmount`,
    /// and refuses a mount in use; `-uz` detaches it even so.
    fn fusermount(how: &str, root: &Path) -> Result<(), String> {
        let run = |program| Command::new(program).arg(how).arg(root).output();
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

    pub fn eject(path: &Path) -> Option<Result<(), String>> {
        let on = disk::sftp::parse(path)?.authority;
        let phones = list_now().ok()?;
        let phone = phones.iter().find(|p| authority(&p.id) == on)?;
        // Its server goes first, or the mount would be in use by it.
        disk::sftp::disconnect(path);
        Some(fusermount("-u", &phone.root))
    }

    #[cfg(test)]
    #[test]
    fn only_some_errors_mean_a_phone_is_cut_off() {
        // What a broken mount says to everything.
        assert!(means_cut_off(libc::EIO));
        assert!(means_cut_off(libc::ENOTCONN));
        // What a healthy one says of a file that isn't there.
        assert!(!means_cut_off(libc::ENOENT));
        assert!(!means_cut_off(libc::EACCES));
    }

    #[cfg(test)]
    #[test]
    fn a_busy_mount_is_explained() {
        let busy = "fusermount3: failed to unmount /run/user/1000/x: Device or resource busy\n";
        assert!(unmount_error(busy).starts_with("the phone is in use"));
        assert_eq!(unmount_error("fusermount3: nope\n"), "fusermount3: nope");
    }

    #[cfg(test)]
    #[test]
    fn a_mounts_connection_is_found() {
        let mounts = "\
            39 1 0:30 / / rw shared:1 - btrfs /dev/sda rw\n\
            833 39 0:216 / /run/user/1000/phone rw,nosuid shared:554 - fuse.sshfs k@h:/ rw\n";
        let found = |root: &str| fuse_connection(mounts, Path::new(root));
        assert_eq!(found("/run/user/1000/phone").as_deref(), Some("216"));
        assert_eq!(found("/run/user/1000/other"), None);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn phone(folders: &[&str]) -> Phone {
        Phone {
            id: "id".to_owned(),
            name: "Pixel".to_owned(),
            root: PathBuf::from("/m"),
            dead_mount: false,
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
        let top = PathBuf::from("sftp://kdeconnect-test/storage/0");
        folders().push((top.clone(), "Pixel".to_owned()));
        let inside = top.join("DCIM");

        assert!(is_phone(&inside));
        assert!(!is_phone(Path::new("sftp://me@host/storage/0")));
        assert!(!is_phone(Path::new("/storage/0")));
        assert_eq!(crate::fs::location_name(&top), "Pixel");
        assert_eq!(crate::fs::display_name(&top), "0");
        assert_eq!(crate::fs::location_name(&inside), "DCIM");
        assert_eq!(crate::remote::parent(&inside), Some(top.as_path()));
        assert_eq!(crate::remote::parent(&top), None);
        assert_eq!(crate::remote::ancestors(&inside).count(), 2);

        let beside = Path::new("sftp://kdeconnect-test/storage/01");
        assert_eq!(root_of(beside), None);
    }

    /// Other people's programs are given the file itself, not a URL they can't
    /// open; a server's stays as it is.
    #[test]
    fn programs_are_given_the_file_in_a_phones_mount() {
        assert_eq!(
            mount_path(Path::new("sftp://kdeconnect-a/run/m/DCIM/v.mp4")),
            Some(PathBuf::from("/run/m/DCIM/v.mp4"))
        );
        assert_eq!(mount_path(Path::new("sftp://me@host/srv/v.mp4")), None);
        assert_eq!(mount_path(Path::new("/home/v.mp4")), None);
    }

    /// A tab saved on the mount itself goes the safe way round.
    #[test]
    fn a_path_in_a_phones_mount_is_reached_as_the_phones() {
        let root = PathBuf::from("/run/phone-mount-test");
        MOUNTS
            .lock()
            .unwrap()
            .push((root.clone(), "kdeconnect-test".to_owned()));
        assert_eq!(
            reached(&root.join("storage")),
            Some(PathBuf::from(
                "sftp://kdeconnect-test/run/phone-mount-test/storage"
            ))
        );
        assert_eq!(reached(Path::new("/run/phone-mount-test2/storage")), None);
        assert_eq!(reached(Path::new("/home")), None);
    }

    /// One that's ejected is still listed, so it isn't mounted again.
    #[test]
    fn only_a_phone_that_vanished_counts_as_dropped() {
        let mounted = [phone(&["a"])];
        let ejected = [phone(&[])];
        assert_eq!(vanished(&mounted, &[]), ["id"]);
        assert!(vanished(&mounted, &ejected).is_empty());
        assert!(vanished(&ejected, &[]).is_empty());
    }

    /// Each try that fails is a notification from KDE Connect, so they're
    /// spaced out further each time, then stop.
    #[test]
    fn mounting_a_dropped_phone_again_is_tried_a_few_times() {
        let t0 = std::time::Instant::now();
        let s = std::time::Duration::from_secs;
        let mut state = State {
            phones: vec![phone(&[])],
            dropped: vec!["id".to_owned()],
            ..State::default()
        };
        assert!(remount_due(&state, "id", t0));

        note_remount(&mut state, "id", false, t0);
        assert!(!remount_due(&state, "id", t0 + s(4)));
        assert!(remount_due(&state, "id", t0 + s(5)));
        note_remount(&mut state, "id", false, t0 + s(5));
        assert!(!remount_due(&state, "id", t0 + s(34)));
        assert!(remount_due(&state, "id", t0 + s(35)));
        note_remount(&mut state, "id", false, t0 + s(35));
        assert!(remount_due(&state, "id", t0 + s(155)));
        note_remount(&mut state, "id", false, t0 + s(155));
        assert!(!remount_due(&state, "id", t0 + s(100_000)), "given up on");

        // It left and came back: reconnected, so it's tried again.
        state.phones.clear();
        forget_remounts_of_unlisted(&mut state);
        state.phones.push(phone(&[]));
        assert!(remount_due(&state, "id", t0 + s(100_000)));

        note_remount(&mut state, "id", false, t0);
        note_remount(&mut state, "id", true, t0);
        assert!(state.remounts.is_empty(), "mounted: starts afresh");
    }

    /// Mounting one by hand that isn't dropped counts for nothing.
    #[test]
    fn only_dropped_phones_count_their_tries() {
        let mut state = State {
            phones: vec![phone(&[])],
            ..State::default()
        };
        note_remount(&mut state, "id", false, std::time::Instant::now());
        assert!(state.remounts.is_empty());
    }

    #[test]
    fn folders_gone_and_new_are_noticed() {
        let both = [phone(&["a", "b"])];
        let one = [phone(&["b"])];
        assert_eq!(difference(&both, &one), [PathBuf::from("/m/a")]);
        assert!(difference(&one, &both).is_empty());
        assert_eq!(difference(&one, &[]), [PathBuf::from("/m/b")]);
    }
}
