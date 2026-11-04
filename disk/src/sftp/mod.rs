//! Files on other computers, over SFTP: paths like
//! `sftp://user@host:port/home/user/notes.txt`. ssh itself connects
//! (`ssh -s host sftp`), so the user's `~/.ssh/config`, keys, agent, known
//! hosts and jump hosts all apply; passwords and new host keys are asked
//! about through [`set_prompter`].
//!
//! A path's first part names the computer as ssh would (`user@host:port`,
//! any part but the host optional), and the rest is where on it. One
//! connection per such name is shared by everything that uses it.
//!
//! What listings say about each file is remembered for half a minute, so
//! that looking at each entry in turn doesn't cost a round trip apiece.
//! Changes made through here are noted for [`take_changes`], as nothing
//! watches the server's folders.

mod askpass;
mod file;
mod session;
mod wire;

use std::{
    cell::Cell,
    collections::{BTreeSet, HashMap},
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError},
    time::{Duration, SystemTime},
};

use web_time::Instant;

pub use file::File;
use session::Session;
pub use wire::{Attrs, S_IFDIR, S_IFLNK, S_IFREG};
use wire::{Reply, Writer};

pub const SCHEME: &str = "sftp://";

/// How long what the server said about a file is believed. Changes made
/// through here are known at once; others' show after this, or a refresh.
const FRESH: Duration = Duration::from_secs(30);

/// A computer to connect to, as ssh is told it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

impl Target {
    /// Reads `user@host:port`; `[...]` around a host allows colons in it.
    pub fn parse(authority: &str) -> Option<Self> {
        let (user, rest) = match authority.rsplit_once('@') {
            Some((user, rest)) => (Some(user.to_owned()), rest),
            None => (None, authority),
        };
        let (host, port) = if let Some(bracketed) = rest.strip_prefix('[') {
            let (host, after) = bracketed.split_once(']')?;
            let port = match after {
                "" => None,
                _ => Some(after.strip_prefix(':')?.parse().ok()?),
            };
            (host, port)
        } else {
            match rest.split_once(':') {
                Some((host, port)) => (host, Some(port.parse().ok()?)),
                None => (rest, None),
            }
        };
        // Nothing ssh would take for an option, or that splits into two.
        let fine = |s: &str| {
            !s.is_empty()
                && !s.starts_with('-')
                && !s.chars().any(|c| c.is_whitespace() || c.is_control())
        };
        if !fine(host) || host.contains('/') || !user.as_deref().is_none_or(fine) {
            return None;
        }
        Some(Self {
            user,
            host: host.to_owned(),
            port,
        })
    }
}

/// A place on a server: which one, and the absolute path there.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Location {
    /// The computer, exactly as the path names it.
    pub authority: String,
    /// Starts with `/`, and has no `.`, `..` or trailing `/`.
    pub path: String,
}

impl Location {
    /// The path the app knows this place by.
    pub fn to_path(&self) -> PathBuf {
        url(&self.authority, &self.path)
    }

    pub fn join(&self, name: &str) -> Self {
        let mut path = self.path.clone();
        if !path.ends_with('/') {
            path.push('/');
        }
        path.push_str(name);
        Self {
            authority: self.authority.clone(),
            path,
        }
    }

    pub fn parent(&self) -> Option<Self> {
        if self.path == "/" {
            return None;
        }
        let cut = self.path.rfind('/').unwrap_or(0);
        Some(Self {
            authority: self.authority.clone(),
            path: if cut == 0 {
                "/".into()
            } else {
                self.path[..cut].into()
            },
        })
    }

    fn is_within(&self, other: &Self) -> bool {
        self.authority == other.authority
            && (self.path == other.path
                || other.path == "/"
                || self
                    .path
                    .strip_prefix(&other.path)
                    .is_some_and(|rest| rest.starts_with('/')))
    }
}

/// The app's path for `path` on the computer `authority`.
pub fn url(authority: &str, path: &str) -> PathBuf {
    let path = if path == "/" { "" } else { path };
    PathBuf::from(format!("{SCHEME}{authority}{path}"))
}

/// Where `path` is, if it's on a server.
pub fn parse(path: &Path) -> Option<Location> {
    let text = path.to_str()?;
    let rest = text.strip_prefix(SCHEME)?;
    // Windows joins paths with `\`.
    let rest = rest.replace('\\', "/");
    let (authority, path) = match rest.find('/') {
        Some(slash) => rest.split_at(slash),
        None => (rest.as_str(), ""),
    };
    Target::parse(authority)?;
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    Some(Location {
        authority: authority.to_owned(),
        path: format!("/{}", parts.join("/")),
    })
}

/// Whether `path` is on a server.
pub fn is_remote(path: &Path) -> bool {
    path.to_str().is_some_and(|s| s.starts_with(SCHEME))
}

/// The top of the server `path` is on, if it is on one: `sftp://host`.
pub fn root(path: &Path) -> Option<PathBuf> {
    parse(path).map(|location| url(&location.authority, "/"))
}

// ---------------------------------------------------------------------------
// Asking the user

/// What ssh wants to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    /// A password, passphrase or code; answered without showing it.
    Secret,
    /// Whether to trust a host key not seen before: "yes" or "no".
    HostKey,
    /// Whether to go ahead (e.g. use a key the agent guards): any answer
    /// says yes.
    Confirm,
    /// Something to show while ssh waits, like touching a security key. It
    /// needs no answer, and ssh takes it down when done.
    Notice,
}

#[derive(Clone, Debug)]
pub struct Prompt {
    /// The computer it's about, as the path names it.
    pub host: String,
    /// ssh's question, as it puts it.
    pub text: String,
    pub kind: PromptKind,
}

type Prompter = Box<dyn Fn(Prompt) -> Option<String> + Send + Sync>;

static PROMPTER: OnceLock<Prompter> = OnceLock::new();

/// Sets who answers ssh's questions: called on a thread of its own, it
/// waits for the answer, or `None` to cancel. Without one, ssh gets no
/// answers, so only keys and agents work. Only the first call counts.
pub fn set_prompter(prompter: impl Fn(Prompt) -> Option<String> + Send + Sync + 'static) {
    let _ = PROMPTER.set(Box::new(prompter));
}

fn prompt(prompt: Prompt) -> Option<String> {
    PROMPTER.get().and_then(|prompter| prompter(prompt))
}

/// Call first thing in `main`: when this process was started as ssh's
/// askpass, it puts ssh's question to the running app, and returns the exit
/// code to quit with.
pub fn askpass_main() -> Option<i32> {
    askpass::main()
}

// ---------------------------------------------------------------------------
// Connections

thread_local! {
    static NO_CONNECTING: Cell<bool> = const { Cell::new(false) };
}

/// Keeps this thread from connecting: something here that would need to
/// fails as not connected instead. For the UI's thread, which mustn't wait
/// on the network, or on the user to answer a prompt it would draw itself.
pub fn never_connect_on_this_thread() {
    NO_CONNECTING.set(true);
}

/// How the connection to a computer is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Disconnected,
    Connecting,
    Connected,
    /// Why it couldn't connect, or why it dropped.
    Failed(String),
}

enum Connection {
    Connecting,
    Up(Arc<Session>),
    Failed(String),
}

#[derive(Default)]
struct Pool {
    connections: HashMap<String, Connection>,
    extra_args: Vec<OsString>,
    program: Option<OsString>,
    askpass: Option<PathBuf>,
}

static POOL: OnceLock<(Mutex<Pool>, Condvar)> = OnceLock::new();

fn pool() -> (MutexGuard<'static, Pool>, &'static Condvar) {
    let (pool, changed) = POOL.get_or_init(Default::default);
    (pool.lock().unwrap_or_else(PoisonError::into_inner), changed)
}

/// Gives ssh these arguments too, before the rest (for tests: `-F config`).
pub fn set_extra_ssh_args(args: impl IntoIterator<Item = impl Into<OsString>>) {
    pool().0.extra_args = args.into_iter().map(Into::into).collect();
}

/// Where connections listen for other ssh's to share them (ssh's
/// `ControlPath`), so a terminal's ssh to a server needs no password again.
/// A folder only the user can reach, in the runtime folder (or `/tmp`: on
/// macOS the temporary folder's path is too long for a socket). `None` on
/// Windows, whose OpenSSH can't share connections, or if the folder isn't
/// safe.
pub fn control_path() -> Option<String> {
    static PATH: OnceLock<Option<String>> = OnceLock::new();
    PATH.get_or_init(|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, MetadataExt};
            let base = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|dir| dir.is_absolute())
                .unwrap_or_else(|| PathBuf::from("/tmp"));
            let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
            let user: String = user.chars().filter(char::is_ascii_alphanumeric).collect();
            let dir = base.join(format!("egui-files-ssh-{user}"));
            let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
            // Ours, and no one else's to get into, or not at all.
            let meta = std::fs::symlink_metadata(&dir).ok()?;
            let home = std::fs::metadata(std::env::var_os("HOME")?).ok()?;
            if !meta.is_dir() || meta.uid() != home.uid() || meta.mode() & 0o077 != 0 {
                return None;
            }
            // %C: a hash of where it leads, short enough for a socket.
            Some(format!("{}/%C", dir.to_str()?))
        }
        #[cfg(not(unix))]
        None
    })
    .clone()
}

/// Runs this instead of `ssh` (for tests).
pub fn set_ssh_program(program: impl Into<OsString>) {
    pool().0.program = Some(program.into());
}

/// Has ssh ask through this program rather than this one started again
/// (for tests, whose program is the test harness). It must call
/// [`askpass_main`].
pub fn set_askpass_program(program: impl Into<PathBuf>) {
    pool().0.askpass = Some(program.into());
}

fn ssh_program() -> OsString {
    pool().0.program.clone().unwrap_or_else(|| "ssh".into())
}

fn not_connected(authority: &str, why: Option<&str>) -> io::Error {
    let message = match why {
        Some(why) => format!("not connected to {authority}: {why}"),
        None => format!("not connected to {authority}"),
    };
    io::Error::new(io::ErrorKind::NotConnected, message)
}

/// The connection to `authority`, connecting if this thread may. A
/// connection that failed stays failed until [`connect`] tries again.
fn session(authority: &str) -> io::Result<Arc<Session>> {
    session_or_connect(authority, false)
}

fn session_or_connect(authority: &str, retry: bool) -> io::Result<Arc<Session>> {
    let may_connect = !NO_CONNECTING.get();
    let (mut pool, changed) = pool();
    loop {
        match pool.connections.get(authority) {
            Some(Connection::Up(session)) if session.is_open() => return Ok(session.clone()),
            Some(Connection::Connecting) if may_connect => {
                pool = changed.wait(pool).unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            Some(Connection::Connecting) => {
                return Err(not_connected(authority, Some("still connecting")));
            }
            Some(Connection::Failed(why)) if !retry => {
                return Err(not_connected(authority, Some(why)));
            }
            // Never connected, dropped, or failed and worth another try.
            _ if may_connect => break,
            _ => return Err(not_connected(authority, None)),
        }
    }
    pool.connections
        .insert(authority.to_owned(), Connection::Connecting);
    let extra_args = pool.extra_args.clone();
    let askpass = pool.askpass.clone();
    drop(pool);

    let started = start_session(authority, &extra_args, askpass);
    let (mut pool, changed) = self::pool();
    let result = match started {
        Ok(session) => {
            let session = Arc::new(session);
            pool.connections
                .insert(authority.to_owned(), Connection::Up(session.clone()));
            Ok(session)
        }
        Err(err) => {
            pool.connections
                .insert(authority.to_owned(), Connection::Failed(err.to_string()));
            Err(err)
        }
    };
    changed.notify_all();
    note_change(&url(authority, "/"));
    result
}

fn start_session(
    authority: &str,
    extra_args: &[OsString],
    askpass: Option<PathBuf>,
) -> io::Result<Session> {
    if cfg!(target_os = "android") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SFTP isn't available on Android yet",
        ));
    }
    let target = Target::parse(authority)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a computer's name"))?;
    let mut command = session::ssh_command(&target);
    if !extra_args.is_empty() {
        // Ahead of the rest, so `-F` and the like come before the host.
        let rest: Vec<OsString> = command.get_args().map(OsString::from).collect();
        let program = command.get_program().to_owned();
        command = std::process::Command::new(program);
        command.args(extra_args).args(rest);
    }
    let askpass = match askpass {
        Some(program) => program,
        None => std::env::current_exe()?,
    };
    let asking = askpass::set_up(&mut command, authority, &askpass)?;
    Session::start(command, &asking)
}

/// Connects to the computer `path` is on, if it isn't already, trying again
/// if it failed before. Waits for the user to answer any questions.
pub fn connect(path: &Path) -> io::Result<()> {
    let location = parse(path).ok_or_else(not_remote)?;
    session_or_connect(&location.authority, true).map(drop)
}

/// Closes the connection to the computer `path` is on.
pub fn disconnect(path: &Path) {
    let Some(location) = parse(path) else { return };
    let (mut pool, changed) = pool();
    if matches!(
        pool.connections.get(&location.authority),
        Some(Connection::Connecting)
    ) {
        return;
    }
    pool.connections.remove(&location.authority);
    changed.notify_all();
    drop(pool);
    forget(&location.to_path());
    note_change(&url(&location.authority, "/"));
}

/// How the connection to the computer `path` is on is.
pub fn status(path: &Path) -> Status {
    let Some(location) = parse(path) else {
        return Status::Disconnected;
    };
    status_of(&location.authority)
}

fn status_of(authority: &str) -> Status {
    match pool().0.connections.get(authority) {
        None => Status::Disconnected,
        Some(Connection::Connecting) => Status::Connecting,
        Some(Connection::Up(session)) if session.is_open() => Status::Connected,
        Some(Connection::Up(_)) => Status::Failed("the connection dropped".into()),
        Some(Connection::Failed(why)) => Status::Failed(why.clone()),
    }
}

/// Each computer connected to, connecting or failed, by its top folder.
pub fn connections() -> Vec<(PathBuf, Status)> {
    let authorities: Vec<String> = pool().0.connections.keys().cloned().collect();
    let mut list: Vec<_> = authorities
        .into_iter()
        .map(|authority| {
            let status = status_of(&authority);
            (url(&authority, "/"), status)
        })
        .collect();
    list.sort_by(|a, b| a.0.cmp(&b.0));
    list
}

/// The user's home folder on the computer `path` is on.
pub fn home(path: &Path) -> io::Result<PathBuf> {
    let location = parse(path).ok_or_else(not_remote)?;
    let session = session(&location.authority)?;
    Ok(url(&location.authority, &session.home))
}

fn not_remote() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not an sftp:// path")
}

// ---------------------------------------------------------------------------
// What's known of files

struct Known {
    at: Instant,
    /// The entry itself, links not followed.
    link: Attrs,
    /// What a link leads to, if it's one and that could be found.
    target: Option<Attrs>,
}

#[derive(Default)]
struct Cache {
    files: HashMap<Location, Known>,
    listings: HashMap<Location, (Instant, Vec<String>)>,
    changes: BTreeSet<PathBuf>,
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

fn cache() -> MutexGuard<'static, Cache> {
    CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn note_change(path: &Path) {
    cache().changes.insert(path.to_path_buf());
}

/// Notes that `location` changed: forgets what was known of it and its
/// folder, and tells [`take_changes`].
fn changed(location: &Location) {
    let mut cache = cache();
    cache.files.remove(location);
    cache.listings.remove(location);
    if let Some(parent) = location.parent() {
        cache.listings.remove(&parent);
    }
    cache.changes.insert(location.to_path());
}

/// Like [`changed`], for everything in `location` too.
fn changed_within(location: &Location) {
    let mut cache = cache();
    cache.files.retain(|known, _| !known.is_within(location));
    cache.listings.retain(|known, _| !known.is_within(location));
    if let Some(parent) = location.parent() {
        cache.listings.remove(&parent);
    }
    cache.changes.insert(location.to_path());
}

/// Forgets what's known of `path` and anything in it, so it's asked about
/// afresh: for refreshing.
pub fn forget(path: &Path) {
    let Some(location) = parse(path) else { return };
    let mut cache = cache();
    cache.files.retain(|known, _| !known.is_within(&location));
    cache
        .listings
        .retain(|known, _| !known.is_within(&location));
}

/// What a listing said, to tell whether it's changed: each name, with its
/// type, size and when it was changed.
type Summary = Vec<(String, Option<u32>, Option<u64>, Option<(u32, u32)>)>;

fn summary(cache: &Cache, location: &Location) -> Option<Summary> {
    let (_, names) = cache.listings.get(location)?;
    let mut summary: Summary = names
        .iter()
        .map(|name| {
            let attrs = cache.files.get(&location.join(name)).map(|k| &k.link);
            (
                name.clone(),
                attrs.and_then(|a| a.mode),
                attrs.and_then(|a| a.size),
                attrs.and_then(|a| a.atime_mtime).map(|(_, m)| (0, m)),
            )
        })
        .collect();
    summary.sort();
    Some(summary)
}

/// Lists the folder at `path` again, if connected, and says whether it
/// changed since it was last listed (others' changes: the app's own are
/// known already). A change is noted for [`take_changes`]. Never connects,
/// so never asks the user anything: for looking now and then.
pub fn relist(path: &Path) -> io::Result<bool> {
    let location = parse(path).ok_or_else(not_remote)?;
    if status_of(&location.authority) != Status::Connected {
        return Err(not_connected(&location.authority, None));
    }
    let before = {
        let mut cache = cache();
        let before = summary(&cache, &location);
        cache.listings.remove(&location);
        before
    };
    read_dir(&location)?;
    let after = summary(&cache(), &location);
    let changed = before.is_some() && before != after;
    if changed {
        note_change(&location.to_path());
    }
    Ok(changed)
}

/// Whether the folder at `path` was listed recently enough that listing it
/// again needs no round trip.
pub fn is_listed(path: &Path) -> bool {
    parse(path).is_some_and(|location| {
        cache()
            .listings
            .get(&location)
            .is_some_and(|(at, _)| at.elapsed() < FRESH)
    })
}

/// Server paths changed through here since the last call.
pub fn take_changes() -> BTreeSet<PathBuf> {
    std::mem::take(&mut cache().changes)
}

fn known(location: &Location, follow: bool) -> Option<Attrs> {
    let cache = cache();
    let known = cache.files.get(location)?;
    if known.at.elapsed() >= FRESH {
        return None;
    }
    if follow && known.link.is_symlink() {
        known.target.clone()
    } else {
        Some(known.link.clone())
    }
}

// ---------------------------------------------------------------------------
// Operations

/// What's at `location`, following links if `follow`.
pub fn stat(location: &Location, follow: bool) -> io::Result<Attrs> {
    if let Some(attrs) = known(location, follow) {
        return Ok(attrs);
    }
    let session = session(&location.authority)?;
    let ask = |kind| {
        session
            .call(kind, Writer::default().str(&location.path))?
            .attrs()
    };
    // The entry itself first, then where it leads if it's a link: so both
    // can be remembered.
    let link = ask(wire::LSTAT)?;
    let target = if link.is_symlink() && follow {
        Some(ask(wire::STAT))
    } else {
        None
    };
    cache().files.insert(
        location.clone(),
        Known {
            at: Instant::now(),
            link: link.clone(),
            target: target.as_ref().and_then(|t| t.as_ref().ok()).cloned(),
        },
    );
    match target {
        Some(target) => target,
        None => Ok(link),
    }
}

/// One thing in a folder.
pub struct Listed {
    pub location: Location,
    pub name: String,
    pub link: Attrs,
    /// What a link leads to, if it leads anywhere.
    pub target: Option<Attrs>,
}

/// What's in the folder at `location`, with what links lead to.
pub fn read_dir(location: &Location) -> io::Result<Vec<Listed>> {
    if let Some(listed) = listed_recently(location) {
        return Ok(listed);
    }
    let session = session(&location.authority)?;
    let handle = session
        .call(wire::OPENDIR, Writer::default().str(&location.path))?
        .handle()?;
    let mut names = Vec::new();
    let result = loop {
        match session.call(wire::READDIR, Writer::default().bytes(&handle)) {
            Ok(Reply::Name(batch)) => names.extend(
                batch
                    .into_iter()
                    .filter(|(name, _)| name != "." && name != ".."),
            ),
            Ok(reply) if reply.is_eof() => break Ok(()),
            Ok(reply) => break Err(reply.error()),
            Err(err) => break Err(err),
        }
    };
    let _ = session.call(wire::CLOSE, Writer::default().bytes(&handle));
    result?;

    // Ask where every link leads at once, rather than one by one.
    let mut asked = Vec::new();
    for (name, attrs) in &names {
        if attrs.is_symlink() {
            let path = location.join(name).path;
            asked.push(session.request(wire::STAT, Writer::default().str(&path))?);
        }
    }
    let mut targets = asked.into_iter();
    let now = Instant::now();
    let listed: Vec<Listed> = names
        .into_iter()
        .map(|(name, link)| {
            let target = if link.is_symlink() {
                targets
                    .next()
                    .and_then(|pending| pending.wait().and_then(Reply::attrs).ok())
            } else {
                None
            };
            Listed {
                location: location.join(&name),
                name,
                link,
                target,
            }
        })
        .collect();
    let mut cache = cache();
    for entry in &listed {
        cache.files.insert(
            entry.location.clone(),
            Known {
                at: now,
                link: entry.link.clone(),
                target: entry.target.clone(),
            },
        );
    }
    let names = listed.iter().map(|entry| entry.name.clone()).collect();
    cache.listings.insert(location.clone(), (now, names));
    Ok(listed)
}

/// The listing remembered for `location`, if recent and nothing in it has
/// been forgotten since.
fn listed_recently(location: &Location) -> Option<Vec<Listed>> {
    let cache = cache();
    let (at, names) = cache.listings.get(location)?;
    if at.elapsed() >= FRESH {
        return None;
    }
    names
        .iter()
        .map(|name| {
            let entry = location.join(name);
            let known = cache.files.get(&entry)?;
            Some(Listed {
                name: name.clone(),
                link: known.link.clone(),
                target: known.target.clone(),
                location: entry,
            })
        })
        .collect()
}

/// How to open a file; see `std::fs::OpenOptions`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Open {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub truncate: bool,
    pub create: bool,
    pub create_new: bool,
    pub mode: Option<u32>,
}

pub fn open(location: &Location, how: Open) -> io::Result<File> {
    let session = session(&location.authority)?;
    let mut flags = 0;
    if how.read {
        flags |= wire::OPEN_READ;
    }
    if how.write || how.append {
        flags |= wire::OPEN_WRITE;
    }
    if how.append {
        flags |= wire::OPEN_APPEND;
    }
    if how.truncate {
        flags |= wire::OPEN_TRUNCATE;
    }
    if how.create || how.create_new {
        flags |= wire::OPEN_CREATE;
    }
    if how.create_new {
        flags |= wire::OPEN_EXCLUSIVE;
    }
    let attrs = Attrs {
        // As std does: the umask applies on the server.
        mode: (how.create || how.create_new).then_some(how.mode.unwrap_or(0o666)),
        ..Attrs::default()
    };
    let request = Writer::default()
        .str(&location.path)
        .u32(flags)
        .attrs(&attrs);
    let handle = match session.call(wire::OPEN, request)? {
        Reply::Handle(handle) => handle,
        reply if reply.is_failure() && how.create_new && exists(&session, location) => {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "File exists"));
        }
        reply => return Err(reply.error()),
    };
    if how.write || how.append || how.truncate || how.create || how.create_new {
        changed(location);
    }
    Ok(File::new(session, handle, location.clone(), how.append))
}

fn exists(session: &Session, location: &Location) -> bool {
    session
        .call(wire::LSTAT, Writer::default().str(&location.path))
        .and_then(Reply::attrs)
        .is_ok()
}

pub fn create_dir(location: &Location) -> io::Result<()> {
    let session = session(&location.authority)?;
    let request = Writer::default().str(&location.path).attrs(&Attrs {
        mode: Some(0o777),
        ..Attrs::default()
    });
    match session.call(wire::MKDIR, request)? {
        reply if reply.is_failure() && exists(&session, location) => {
            Err(io::Error::new(io::ErrorKind::AlreadyExists, "File exists"))
        }
        reply => reply.ok(),
    }?;
    changed(location);
    Ok(())
}

pub fn remove_file(location: &Location) -> io::Result<()> {
    let session = session(&location.authority)?;
    session
        .call(wire::REMOVE, Writer::default().str(&location.path))?
        .ok()?;
    changed(location);
    Ok(())
}

pub fn remove_dir(location: &Location) -> io::Result<()> {
    let session = session(&location.authority)?;
    match session.call(wire::RMDIR, Writer::default().str(&location.path))? {
        reply if reply.is_failure() && exists(&session, location) => Err(io::Error::new(
            io::ErrorKind::DirectoryNotEmpty,
            "Directory not empty",
        )),
        reply => reply.ok(),
    }?;
    changed(location);
    Ok(())
}

pub fn remove_dir_all(location: &Location) -> io::Result<()> {
    if !stat(location, false)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "Not a directory",
        ));
    }
    remove_tree(location)?;
    changed_within(location);
    Ok(())
}

fn remove_tree(location: &Location) -> io::Result<()> {
    // Not from the cache: it may have changed since it was listed.
    forget(&location.to_path());
    for entry in read_dir(location)? {
        if entry.link.is_dir() {
            remove_tree(&entry.location)?;
        } else {
            remove_file(&entry.location)?;
        }
    }
    remove_dir(location)
}

/// Moves `from` to `to` on the same computer, replacing a file there as
/// `std::fs::rename` does on Unix.
pub fn rename(from: &Location, to: &Location) -> io::Result<()> {
    if from.authority != to.authority {
        return Err(io::Error::new(
            io::ErrorKind::CrossesDevices,
            "they're on different computers",
        ));
    }
    let session = session(&from.authority)?;
    let request = Writer::default().str(&from.path).str(&to.path);
    let reply = if session.has_extension("posix-rename@openssh.com") {
        session
            .extended("posix-rename@openssh.com", request)?
            .wait()?
    } else {
        session.call(wire::RENAME, request)?
    };
    match reply {
        reply if reply.is_failure() && exists(&session, to) => {
            Err(io::Error::new(io::ErrorKind::AlreadyExists, "File exists"))
        }
        reply => reply.ok(),
    }?;
    changed_within(from);
    changed_within(to);
    Ok(())
}

pub fn read_link(location: &Location) -> io::Result<PathBuf> {
    let session = session(&location.authority)?;
    let target = session
        .call(wire::READLINK, Writer::default().str(&location.path))?
        .name()?;
    Ok(PathBuf::from(target))
}

/// Makes a link at `link` leading to `target`, which is kept as it's given.
pub fn symlink(target: &Path, link: &Location) -> io::Result<()> {
    let session = session(&link.authority)?;
    let target = target.to_string_lossy().replace('\\', "/");
    // OpenSSH has the two the other way around from the draft standard,
    // and every server since copies it.
    let request = Writer::default().str(&target).str(&link.path);
    session.call(wire::SYMLINK, request)?.ok()?;
    changed(link);
    Ok(())
}

pub fn hard_link(original: &Location, link: &Location) -> io::Result<()> {
    if original.authority != link.authority {
        return Err(io::Error::new(
            io::ErrorKind::CrossesDevices,
            "they're on different computers",
        ));
    }
    let session = session(&link.authority)?;
    if !session.has_extension("hardlink@openssh.com") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the server can't make hard links",
        ));
    }
    let request = Writer::default().str(&original.path).str(&link.path);
    session
        .extended("hardlink@openssh.com", request)?
        .wait()?
        .ok()?;
    changed(link);
    Ok(())
}

/// The path with links followed, as the server sees it.
pub fn canonicalize(location: &Location) -> io::Result<Location> {
    let session = session(&location.authority)?;
    let path = session
        .call(wire::REALPATH, Writer::default().str(&location.path))?
        .name()?;
    Ok(Location {
        authority: location.authority.clone(),
        path,
    })
}

fn set_attrs(location: &Location, attrs: &Attrs) -> io::Result<()> {
    let session = session(&location.authority)?;
    let request = Writer::default().str(&location.path).attrs(attrs);
    session.call(wire::SETSTAT, request)?.ok()?;
    changed(location);
    Ok(())
}

pub fn set_mode(location: &Location, mode: u32) -> io::Result<()> {
    set_attrs(
        location,
        &Attrs {
            mode: Some(mode & 0o7777),
            ..Attrs::default()
        },
    )
}

/// Sets when it was last read (if given; else kept) and changed.
pub fn set_times(
    location: &Location,
    accessed: Option<SystemTime>,
    modified: SystemTime,
) -> io::Result<()> {
    let accessed = match accessed {
        Some(time) => wire::seconds(time),
        None => stat(location, true)?
            .atime_mtime
            .map_or_else(|| wire::seconds(modified), |(a, _)| a),
    };
    set_attrs(
        location,
        &Attrs {
            atime_mtime: Some((accessed, wire::seconds(modified))),
            ..Attrs::default()
        },
    )
}

/// Space on the filesystem holding `location`: (total, free to the user)
/// in bytes, if the server says.
pub fn space(location: &Location) -> io::Result<(u64, u64)> {
    let session = session(&location.authority)?;
    if !session.has_extension("statvfs@openssh.com") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the server doesn't say how much space it has",
        ));
    }
    let reply = session
        .extended("statvfs@openssh.com", Writer::default().str(&location.path))?
        .wait()?;
    let Reply::Extended(body) = reply else {
        return Err(reply.error());
    };
    let mut r = wire::Reader(&body);
    let _block_size = r.u64()?;
    let fragment_size = r.u64()?;
    let blocks = r.u64()?;
    let _free = r.u64()?;
    let available = r.u64()?;
    Ok((
        blocks.saturating_mul(fragment_size),
        available.saturating_mul(fragment_size),
    ))
}

#[cfg(test)]
mod tests;
