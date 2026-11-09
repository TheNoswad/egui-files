//! The Proton Drive account: signing in, keeping signed in, and reading the
//! Drive for `disk::proton`, which is what the rest of the app goes through;
//! and hearing from Proton what others changed there.
//!
//! Proton's SDK is async, so it has a small runtime of its own here, and
//! everything that calls it is on some other thread, never the UI's. The
//! password isn't kept. What is, in the OS's keyring and nowhere else, are
//! the session's tokens and the passphrases that unlock the account's keys.

mod dialog;
mod keyring;
mod trash;

use std::{
    future::Future,
    io,
    sync::{Arc, LazyLock, Mutex, PoisonError},
};

use disk::proton::{Backend, Node, Reader};
use proton_drive_rs::{
    DriveEvent, DriveEventScopeId, EventManager, MemoryCursorStore, ModificationTime, NodeKind,
    ProtonDriveClient, RevisionReader, ThumbnailType,
};
use proton_sdk::{
    account::KeyPassphrases,
    api::{HumanVerification, HumanVerificationCredential, ResponseCode},
    config::ProtonClientConfiguration,
    error::ProtonError,
    ids::{LinkId, NodeUid, SessionId, UserId, VolumeId},
    session::{PasswordMode, ProtonApiSession, ResumeParameters},
};
use zeroize::Zeroizing;

pub use dialog::{open as open_dialog, ui as dialog_ui};
use keyring::Saved;
pub use trash::{open as open_trash, ui as trash_ui};

/// How the app tells Proton who it is: this shape is required, and one that
/// doesn't fit gets requests refused as unusual activity. The channel stays
/// `alpha` while there are only nightlies. Hyphens separate the sections
/// Proton reads, so the app's name can't have one: it answers "Invalid
/// section name" to every request otherwise.
const APP_VERSION: &str = concat!(
    "external-drive-eguifiles@",
    env!("CARGO_PKG_VERSION"),
    "-alpha"
);
const USER_AGENT: &str = concat!("egui-files/", env!("CARGO_PKG_VERSION"));

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("proton-drive")
        .enable_all()
        .build()
        .expect("couldn't start Proton Drive's runtime")
});

/// Waits for `future`. Never on the UI's thread, or one of the runtime's.
fn wait<T>(future: impl Future<Output = T>) -> T {
    RUNTIME.block_on(future)
}

fn config() -> ProtonClientConfiguration {
    ProtonClientConfiguration::new(APP_VERSION).with_user_agent(USER_AGENT)
}

/// Who's signed in.
struct Account {
    session: ProtonApiSession,
    client: ProtonDriveClient,
    /// Hears what changed on the Drive; stopped on signing out.
    events: tokio::task::JoinHandle<()>,
}

static ACCOUNT: Mutex<Option<Account>> = Mutex::new(None);

/// How full the Drive is, as last heard.
static SPACE: Mutex<Option<crate::places::Space>> = Mutex::new(None);
static SPACE_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Held while signing in with what's saved, so that the folders of several
/// tabs don't each do it.
static RESUMING: Mutex<()> = Mutex::new(());

/// What went wrong, in words for the user.
fn describe(err: &ProtonError) -> String {
    match err {
        ProtonError::Api(api) => match api.code {
            ResponseCode::IncorrectLoginCredentials => "The email or password is wrong.".to_owned(),
            ResponseCode::HumanVerificationRequired => {
                "Proton wants a CAPTCHA solved, but didn't say how. Try again later.".to_owned()
            }
            ResponseCode::Blocked | ResponseCode::TooManyRequests => {
                "Proton is refusing sign-ins from here for now. Try again later.".to_owned()
            }
            _ if api.message.is_empty() => format!("Proton said no ({:?}).", api.code),
            _ => api.message.clone(),
        },
        ProtonError::Transport(_) => "Couldn't reach Proton. Is the network up?".to_owned(),
        ProtonError::KeysLocked => "That password didn't unlock the account's keys.".to_owned(),
        other => other.to_string(),
    }
}

/// Whether Proton says the session is over, as opposed to being unreachable.
fn session_ended(err: &ProtonError) -> bool {
    match err {
        ProtonError::KeysLocked => true,
        ProtonError::Api(api) => matches!(
            api.code,
            ResponseCode::Unauthorized | ResponseCode::InvalidRefreshToken
        ),
        _ => false,
    }
}

fn to_io(err: ProtonError) -> io::Error {
    let kind = match &err {
        ProtonError::Transport(_) => io::ErrorKind::ConnectionAborted,
        _ if session_ended(&err) => io::ErrorKind::PermissionDenied,
        ProtonError::Api(api) if api.code == ResponseCode::DoesNotExist => io::ErrorKind::NotFound,
        ProtonError::Api(api) if api.code == ResponseCode::AlreadyExists => {
            io::ErrorKind::AlreadyExists
        }
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, describe(&err))
}

fn parse_uid(id: &str) -> io::Result<NodeUid> {
    let (volume, link) = id
        .split_once('~')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a Drive file's id"))?;
    Ok(NodeUid::new(VolumeId::from(volume), LinkId::from(link)))
}

fn node_of(node: proton_drive_rs::Node) -> Node {
    let (is_dir, size) = match node.kind {
        NodeKind::Folder => (true, 0),
        NodeKind::File {
            claimed_size,
            total_size_on_storage,
            ..
        } => (
            false,
            u64::try_from(claimed_size.unwrap_or(total_size_on_storage)).unwrap_or(0),
        ),
    };
    Node {
        id: node.uid.to_string(),
        name: node.name,
        is_dir,
        size,
        modified: node.modification_time,
    }
}

/// What the Drive is told a file is, by its name.
fn media_type_for(name: &str) -> &'static str {
    let extension = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    match extension.as_deref() {
        Some("txt" | "log") => "text/plain",
        Some("md" | "markdown") => "text/markdown",
        Some("html" | "htm") => "text/html",
        Some("css") => "text/css",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("gz") => "application/gzip",
        Some("tar") => "application/x-tar",
        Some("7z") => "application/x-7z-compressed",
        Some("doc") => "application/msword",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("odt") => "application/vnd.oasis.opendocument.text",
        Some("ods") => "application/vnd.oasis.opendocument.spreadsheet",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("bmp") => "image/bmp",
        Some("heic") => "image/heic",
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("ogg") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("m4a") => "audio/mp4",
        Some("mp4" | "m4v") => "video/mp4",
        Some("mkv") => "video/x-matroska",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        Some("avi") => "video/x-msvideo",
        _ => "application/octet-stream",
    }
}

fn modification_time(seconds: Option<i64>) -> Option<ModificationTime> {
    chrono::DateTime::from_timestamp(seconds?, 0).map(|time| time.fixed_offset())
}

/// The first of a batch's failures, if any did.
fn first_failure(results: Vec<(NodeUid, Result<(), ProtonError>)>) -> Result<(), ProtonError> {
    results.into_iter().try_for_each(|(_, result)| result)
}

struct Drive(ProtonDriveClient);

impl Drive {
    /// A node as it is now. Not `get_node`, which answers from a cache that
    /// only the event feed clears: right after a write, it would tell of the
    /// file as it was, and the editor take the truth for someone else's change
    /// once the feed catches up.
    fn node(&self, uid: &NodeUid) -> io::Result<Node> {
        wait(self.0.enumerate_nodes(std::slice::from_ref(uid)))
            .map_err(to_io)?
            .into_iter()
            .next()
            .map(node_of)
            .ok_or_else(|| io::ErrorKind::NotFound.into())
    }
}

impl Backend for Drive {
    fn root(&self) -> io::Result<Node> {
        wait(self.0.get_my_files_folder())
            .map(node_of)
            .map_err(to_io)
    }

    fn list(&self, folder_id: &str) -> io::Result<Vec<Node>> {
        let folder = parse_uid(folder_id)?;
        let nodes = wait(async {
            let uids = self.0.enumerate_folder_children_node_uids(&folder).await?;
            self.0.enumerate_nodes(&uids).await
        })
        .map_err(to_io)?;
        Ok(nodes
            .into_iter()
            .filter(|node| !node.trashed)
            .map(node_of)
            .collect())
    }

    fn open(&self, file_id: &str) -> io::Result<Box<dyn Reader>> {
        let file = parse_uid(file_id)?;
        let reader = wait(self.0.open_revision(&file)).map_err(to_io)?;
        Ok(Box::new(Revision(reader)))
    }

    fn thumbnail(&self, file_id: &str) -> io::Result<Option<Vec<u8>>> {
        let file = parse_uid(file_id)?;
        wait(self.0.download_thumbnail(&file, ThumbnailType::Thumbnail)).map_err(to_io)
    }

    fn create_folder(&self, parent_id: &str, name: &str) -> io::Result<Node> {
        let parent = parse_uid(parent_id)?;
        let folder = wait(self.0.create_folder(&parent, name, None)).map_err(to_io)?;
        self.node(&folder)
    }

    fn upload(
        &self,
        parent_id: &str,
        name: &str,
        reader: &mut (dyn io::Read + Send),
        size: u64,
        modified: Option<i64>,
    ) -> io::Result<Node> {
        let parent = parse_uid(parent_id)?;
        // A cancelled or crashed upload leaves a draft of its name behind,
        // which would otherwise block this one. Never replaces a finished
        // file: that's refused as the name being taken.
        let file = wait(self.0.upload_file_replacing_draft_from(
            &parent,
            name,
            media_type_for(name),
            reader,
            size as i64,
            Vec::new(),
            modification_time(modified),
            false,
        ))
        .map_err(to_io)?;
        self.node(&file)
    }

    fn upload_revision(
        &self,
        file_id: &str,
        reader: &mut (dyn io::Read + Send),
        size: u64,
        modified: Option<i64>,
    ) -> io::Result<Node> {
        let file = parse_uid(file_id)?;
        wait(self.0.upload_new_revision_from(
            &file,
            reader,
            size as i64,
            Vec::new(),
            modification_time(modified),
        ))
        .map_err(to_io)?;
        self.node(&file)
    }

    fn rename(&self, id: &str, name: &str, is_dir: bool) -> io::Result<()> {
        let node = parse_uid(id)?;
        let media_type = (!is_dir).then(|| media_type_for(name));
        wait(self.0.rename_node(&node, name, media_type)).map_err(to_io)
    }

    fn move_to(&self, id: &str, parent_id: &str) -> io::Result<()> {
        let (node, parent) = (parse_uid(id)?, parse_uid(parent_id)?);
        wait(self.0.move_node(&node, &parent)).map_err(to_io)
    }

    fn copy(&self, id: &str, parent_id: &str, name: &str) -> io::Result<Node> {
        let (node, parent) = (parse_uid(id)?, parse_uid(parent_id)?);
        let copy = wait(self.0.copy_node(&node, &parent, Some(name))).map_err(to_io)?;
        self.node(&copy)
    }

    fn trash(&self, id: &str) -> io::Result<()> {
        let node = parse_uid(id)?;
        let results = wait(self.0.trash_nodes(std::slice::from_ref(&node))).map_err(to_io)?;
        first_failure(results).map_err(to_io)
    }
}

struct Revision(RevisionReader);

impl Reader for Revision {
    fn size(&self) -> u64 {
        self.0.size()
    }

    fn read_at(&self, offset: u64, length: u64) -> io::Result<Vec<u8>> {
        wait(self.0.read_at(offset, length)).map_err(to_io)
    }
}

/// Keeps what's in the keyring up with the session's tokens. Each refresh
/// spends the last refresh token, so a copy that falls behind is dead at the
/// next start. Saved off the runtime, where the keyring's blocking calls can't
/// go; always the latest, should two refreshes' threads run out of order.
fn keep_tokens_saved(session: &ProtonApiSession, saved: Saved) {
    let saved = Arc::new(Mutex::new(saved));
    session.http().set_on_tokens_refreshed(move |tokens| {
        {
            let mut saved = saved.lock().unwrap_or_else(PoisonError::into_inner);
            saved.access_token = tokens.access_token;
            saved.refresh_token = tokens.refresh_token;
        }
        let saved = saved.clone();
        crate::background::spawn("save Proton Drive's sign-in", move || {
            let saved = saved.lock().unwrap_or_else(PoisonError::into_inner);
            if let Err(why) = keyring::save(&saved) {
                super::notify(format!("Couldn't save the Proton Drive sign-in: {why}"));
            }
        });
    });
}

/// Puts the account to use: from here on, Drive paths work.
fn install(session: ProtonApiSession, client: ProtonDriveClient) {
    let events = start_events(client.clone());
    *ACCOUNT.lock().unwrap_or_else(PoisonError::into_inner) = Some(Account {
        session,
        client: client.clone(),
        events,
    });
    disk::proton::set_backend(Some(Arc::new(Drive(client))));
    crate::background::spawn("ask how full Proton Drive is", refresh_space);
    super::retry_within(&disk::proton::top());
    super::repaint();
}

/// Asks how full the Drive is. Waits on the network.
fn refresh_space() {
    const TRIES: u32 = 3;
    let Ok(client) = client() else { return };
    for attempt in 1..=TRIES {
        match wait(client.quota()) {
            Ok(quota) => {
                *SPACE.lock().unwrap_or_else(PoisonError::into_inner) =
                    Some(crate::places::Space {
                        total: u64::try_from(quota.max_space).unwrap_or(0),
                        available: u64::try_from(quota.available()).unwrap_or(0),
                    });
                *SPACE_ERROR.lock().unwrap_or_else(PoisonError::into_inner) = None;
                break;
            }
            Err(err) if attempt == TRIES => {
                *SPACE_ERROR.lock().unwrap_or_else(PoisonError::into_inner) = Some(describe(&err));
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_secs(2)),
        }
    }
    super::repaint();
}

/// Why it isn't known how full the Drive is, if it isn't.
pub fn space_error() -> Option<String> {
    SPACE_ERROR
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// How full the Drive is, for the sidebar.
pub fn space() -> Option<crate::places::Space> {
    SPACE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|space| crate::places::Space {
            total: space.total,
            available: space.available,
        })
}

fn client() -> Result<ProtonDriveClient, String> {
    ACCOUNT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|account| account.client.clone())
        .ok_or_else(|| "Not signed in to Proton Drive.".to_owned())
}

/// Listens to the Drive's own feed of changes, as Proton asks clients to,
/// and has the folders it names fetched again.
fn start_events(client: ProtonDriveClient) -> tokio::task::JoinHandle<()> {
    use tokio::sync::broadcast::error::RecvError;

    // Fetching waits on the network, so it's on a thread of its own, which
    // ends as this task does and its sender goes. `None` is every folder.
    let (changed, folders) = std::sync::mpsc::channel::<Option<String>>();
    crate::background::spawn("follow Proton Drive's changes", move || {
        while let Ok(first) = folders.recv() {
            let mut batch = vec![first];
            batch.extend(folders.try_iter());
            let mut any = false;
            if batch.contains(&None) {
                any |= disk::proton::folder_changed(None);
            } else {
                batch.sort();
                batch.dedup();
                for folder in batch.iter().flatten() {
                    any |= disk::proton::folder_changed(Some(folder));
                }
            }
            refresh_space();
            if any {
                super::repaint();
            }
        }
    });
    RUNTIME.spawn(async move {
        let Ok(volume) = client.main_volume_id().await else {
            return;
        };
        let manager = Arc::new(EventManager::new(
            client,
            DriveEventScopeId::new(volume),
            Arc::new(MemoryCursorStore::default()),
        ));
        let mut events = manager.subscribe();
        // Stops with this task, as a JoinSet ends what's in it when dropped.
        let mut polling = tokio::task::JoinSet::new();
        polling.spawn(async move { manager.run().await });
        loop {
            let folder = match events.recv().await {
                Ok(
                    DriveEvent::NodeUpdated {
                        parent_node_uid, ..
                    }
                    | DriveEvent::NodeDeleted {
                        parent_node_uid, ..
                    },
                ) => parent_node_uid.map(|uid| uid.to_string()),
                Ok(DriveEvent::ContinuityLost { .. }) | Err(RecvError::Lagged(_)) => None,
                Ok(_) => continue,
                Err(RecvError::Closed) => break,
            };
            if changed.send(folder).is_err() {
                break;
            }
        }
    })
}

/// Signs in with what's saved, if not signed in already; asks the user if
/// that can't be done. Waits on the network and the keyring.
pub fn ensure_signed_in() -> Result<(), String> {
    if disk::proton::is_signed_in() {
        return Ok(());
    }
    let _only_one = RESUMING.lock().unwrap_or_else(PoisonError::into_inner);
    if disk::proton::is_signed_in() {
        return Ok(());
    }
    let saved = match keyring::load() {
        Ok(Some(saved)) => saved,
        Ok(None) => {
            dialog::open();
            return Err("Not signed in to Proton Drive.".to_owned());
        }
        Err(why) => {
            dialog::open();
            return Err(format!("Couldn't read the saved sign-in: {why}"));
        }
    };
    let session = ProtonApiSession::resume(
        config(),
        ResumeParameters {
            session_id: SessionId::from(saved.session_id.clone()),
            username: saved.username.clone(),
            user_id: UserId::from(saved.user_id.clone()),
            access_token: saved.access_token.clone(),
            refresh_token: saved.refresh_token.clone(),
            scopes: saved.scopes.clone(),
            is_waiting_for_second_factor_code: false,
            password_mode: if saved.dual_password {
                PasswordMode::Dual
            } else {
                PasswordMode::Single
            },
        },
    )
    .map_err(|err| describe(&err))?;
    let passphrases: KeyPassphrases = saved.passphrases().map_err(str::to_owned)?;
    // Before anything's sent: the first request is what finds an old token
    // expired and replaces it.
    keep_tokens_saved(&session, saved);
    let client = ProtonDriveClient::with_key_passphrases(&session, passphrases);
    if let Err(err) = wait(client.get_my_files_folder()) {
        if session_ended(&err) {
            let _ = keyring::delete();
            dialog::open();
            return Err("Signed out of Proton Drive: sign in again.".to_owned());
        }
        return Err(describe(&err));
    }
    install(session, client);
    Ok(())
}

/// How signing in goes on, after a step of it.
enum Step {
    /// Proton wants a CAPTCHA solved before it'll go on.
    Verify(HumanVerification),
    /// The account has an authenticator app: its code is wanted.
    Code(ProtonApiSession),
    /// The account has a second password for its data.
    Mailbox(ProtonApiSession),
    Done,
}

/// Proton's page for solving `challenge`, for the user's own browser. A
/// browser tab has nothing to hand the page's answer to, so this is the full
/// page, not the embedded one the SDK's URL is for; Proton's Bridge opens the
/// same.
fn browser_url(challenge: &HumanVerification) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode as encode};
    let methods: Vec<String> = challenge
        .methods
        .iter()
        .map(|method| encode(method, NON_ALPHANUMERIC).to_string())
        .collect();
    format!(
        "https://verify.proton.me/?methods={}&token={}",
        methods.join(","),
        encode(&challenge.token, NON_ALPHANUMERIC)
    )
}

/// What a sign-in carries once `challenge` is solved in a browser: its own
/// token, which solving it marks as verified at Proton, and every method on
/// offer, as the user may have used any of them. As Proton's Bridge does.
fn browser_credential(challenge: &HumanVerification) -> HumanVerificationCredential {
    HumanVerificationCredential {
        token: challenge.token.clone(),
        method: challenge.methods.join(","),
    }
}

/// The first step: email and password, and the CAPTCHA solved if one was
/// asked for. It starts over each time, as a refused attempt can't go on.
fn begin(
    email: &str,
    password: &Zeroizing<String>,
    solved: Option<&HumanVerification>,
) -> Result<Step, String> {
    let credential = solved.map(browser_credential);
    let session = match wait(ProtonApiSession::begin_verified(
        config(),
        email,
        password.as_bytes(),
        credential.as_ref(),
    )) {
        Ok(session) => session,
        Err(err) => {
            if let ProtonError::Api(api) = &err
                && let Some(challenge) = api.human_verification()
            {
                return Ok(Step::Verify(challenge));
            }
            return Err(describe(&err));
        }
    };
    if session.is_waiting_for_second_factor() {
        if !session.second_factor_methods().totp {
            return Err(
                "This account only has a security key for its second factor, which this app \
                 can't use yet. Adding an authenticator app to the account would let it sign in."
                    .to_owned(),
            );
        }
        return Ok(Step::Code(session));
    }
    after_second_factor(session, password)
}

/// The authenticator app's code.
fn submit_code(
    mut session: ProtonApiSession,
    code: &str,
    password: &Zeroizing<String>,
) -> Result<Step, String> {
    wait(session.apply_second_factor_code(code)).map_err(|err| describe(&err))?;
    after_second_factor(session, password)
}

fn after_second_factor(
    session: ProtonApiSession,
    password: &Zeroizing<String>,
) -> Result<Step, String> {
    if session.password_mode() == PasswordMode::Dual {
        return Ok(Step::Mailbox(session));
    }
    finish(session, password)
}

/// Unlocks the account with the password for its data, and keeps it so.
fn finish(session: ProtonApiSession, secret: &Zeroizing<String>) -> Result<Step, String> {
    let client = ProtonDriveClient::new(&session, secret.as_bytes().to_vec());
    let passphrases = wait(async {
        client.account().unlock().await?;
        client.get_my_files_folder().await?;
        client.account().key_passphrases().await
    })
    .map_err(|err| describe(&err))?;
    let tokens = wait(session.current_tokens());
    let saved = Saved::new(&session, &tokens, &passphrases);
    // Still signed in if this fails, only not next time.
    if let Err(why) = keyring::save(&saved) {
        super::notify(format!(
            "Signed in, but this computer's keyring couldn't keep the sign-in, so you'll be \
             asked again next time: {why}"
        ));
    }
    keep_tokens_saved(&session, saved);
    install(session, client);
    Ok(Step::Done)
}

/// Connects: with what's saved if it can, or else by asking to sign in.
pub fn connect() {
    if username().is_some() {
        return;
    }
    crate::background::spawn("connect to Proton Drive", || {
        if let Err(why) = ensure_signed_in()
            && !dialog::is_open()
        {
            super::notify(format!("Couldn't connect to Proton Drive: {why}"));
        }
    });
}

/// Signs out: ends the session with Proton, and forgets it here, along with
/// the copies of the Drive's files fetched to open in other apps. Its row goes
/// at once, so the next frame doesn't see it signed in and keep it.
pub fn sign_out() {
    let account = ACCOUNT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    *SPACE.lock().unwrap_or_else(PoisonError::into_inner) = None;
    disk::proton::set_backend(None);
    super::set_cloud(super::Cloud {
        connected: false,
        ..super::cloud()
    });
    crate::background::spawn("sign out of Proton Drive", move || {
        // Forgotten whether or not Proton hears of it.
        if let Err(why) = keyring::delete() {
            super::notify(format!("Couldn't remove the saved sign-in: {why}"));
        }
        if let Err(why) = super::copies::forget_proton() {
            super::notify(format!(
                "Couldn't remove the copies of Proton Drive's files: {why}"
            ));
        }
        if let Some(account) = account {
            account.events.abort();
            let _ = wait(account.session.end());
        }
        super::retry_within(&disk::proton::top());
        super::repaint();
    });
}

/// Who's signed in, for the sidebar.
pub fn username() -> Option<String> {
    ACCOUNT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|account| account.session.username().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_version_has_the_sections_proton_wants() {
        let (name, version) = APP_VERSION.split_once('@').unwrap();
        let sections: Vec<&str> = name.split('-').collect();
        assert_eq!(sections.len(), 3, "{name}");
        assert_eq!(&sections[..2], ["external", "drive"]);
        assert!(sections[2].chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(version.ends_with("-alpha"));
    }

    #[test]
    fn files_are_given_a_type_by_their_name() {
        assert_eq!(media_type_for("a.PDF"), "application/pdf");
        assert_eq!(media_type_for("notes.tar.gz"), "application/gzip");
        assert_eq!(media_type_for("noextension"), "application/octet-stream");
    }

    #[test]
    fn a_captcha_is_solved_in_the_browser_and_its_token_replayed() {
        let challenge = HumanVerification {
            token: "ab+c/d=".to_owned(),
            methods: vec!["captcha".to_owned(), "email".to_owned()],
        };
        assert_eq!(
            browser_url(&challenge),
            "https://verify.proton.me/?methods=captcha,email&token=ab%2Bc%2Fd%3D"
        );
        let credential = browser_credential(&challenge);
        assert_eq!(credential.token, "ab+c/d=");
        assert_eq!(credential.method, "captcha,email");
    }
}
