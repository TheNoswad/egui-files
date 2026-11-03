//! The web demo's files: a home folder, from `web/demo-files/` on the server.
//!
//! `web/demo-files.sh` puts them beside the page with a manifest, which is
//! read at startup, so they're listed right away. A folder's files are fetched
//! in the background when it's shown ([`fetch_folder`]), and pictures wait for
//! theirs ([`is_here`]), so the page keeps going. Anything opened before its
//! file is here is fetched then, which stops the page until it's done. Nothing
//! is kept: a reload starts over.

use std::{
    cell::RefCell,
    collections::{HashSet, VecDeque},
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use js_sys::Uint8Array;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::Response;

pub const HOME: &str = "/home/demo";

/// Beside the page: where web/demo-files.sh puts them.
const FILES: &str = "files";

/// Background fetches at once. Browsers allow six connections to a server over
/// HTTP/1.1, so this leaves room for opening a file, which can't wait for one.
const MAX_FETCHES: usize = 4;

#[derive(Default)]
struct Fetches {
    /// Next first.
    waiting: VecDeque<PathBuf>,
    /// By URL, which stays with a file that moves.
    running: HashSet<String>,
    /// Not tried again in the background: opening the file still does.
    failed: HashSet<String>,
    ctx: Option<egui::Context>,
}

thread_local! {
    static FETCHES: RefCell<Fetches> = RefCell::default();
}

/// Sets what's repainted as files come in, for pictures waiting for them.
pub fn set_context(ctx: &egui::Context) {
    FETCHES.with_borrow_mut(|fetches| fetches.ctx = Some(ctx.clone()));
}

/// Fetches the files in `dir` in the background, ahead of any already waiting,
/// and the smallest first, so that as many as can be are here soonest.
pub fn fetch_folder(dir: &Path) {
    let Ok(entries) = disk::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(u64, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| disk::remote_url(path).is_some())
        .map(|path| (disk::metadata(&path).map_or(0, |m| m.len()), path))
        .collect();
    files.sort();
    FETCHES.with_borrow_mut(|fetches| {
        fetches
            .waiting
            .retain(|path| !files.iter().any(|(_, file)| file == path));
        for (_, path) in files.into_iter().rev() {
            fetches.waiting.push_front(path);
        }
    });
    start_fetches();
}

/// Whether reading the file at `path` won't stop the page to fetch it. If it
/// would, it's fetched in the background, next, and repaints when it's here.
///
/// A file that failed there counts as here: reading it tries once more, and
/// has the error to show.
pub fn is_here(path: &Path) -> bool {
    let Some(url) = disk::remote_url(path) else {
        return true;
    };
    let waits = FETCHES.with_borrow_mut(|fetches| {
        if fetches.failed.contains(&url) {
            return false;
        }
        if !fetches.running.contains(&url) && !fetches.waiting.iter().any(|p| p == path) {
            fetches.waiting.push_front(path.to_owned());
        }
        true
    });
    start_fetches();
    !waits
}

/// Starts the next fetches, up to the limit.
fn start_fetches() {
    FETCHES.with_borrow_mut(|fetches| {
        while fetches.running.len() < MAX_FETCHES {
            let Some(path) = fetches.waiting.pop_front() else {
                return;
            };
            // It may have been opened, written over or moved since.
            let Some(url) = disk::remote_url(&path) else {
                continue;
            };
            if !fetches.failed.contains(&url) && fetches.running.insert(url.clone()) {
                wasm_bindgen_futures::spawn_local(fetch_in_background(url));
            }
        }
    });
}

async fn fetch_in_background(url: String) {
    let result = fetch_async(&url).await;
    let ctx = FETCHES.with_borrow_mut(|fetches| {
        fetches.running.remove(&url);
        if let Err(err) = &result {
            log::warn!("couldn't fetch {url}: {err:?}");
            fetches.failed.insert(url.clone());
        }
        fetches.ctx.clone()
    });
    if let Ok(bytes) = result {
        disk::fill_remote(&url, bytes);
    }
    start_fetches();
    if let Some(ctx) = ctx {
        ctx.request_repaint();
    }
}

async fn fetch_async(url: &str) -> Result<Vec<u8>, JsValue> {
    let window = web_sys::window().ok_or("no window")?;
    let response: Response = JsFuture::from(window.fetch_with_str(url))
        .await?
        .dyn_into()?;
    if !response.ok() {
        return Err(format!("HTTP {}", response.status()).into());
    }
    let buffer = JsFuture::from(response.array_buffer()?).await?;
    Ok(Uint8Array::new(&buffer).to_vec())
}

/// Makes the demo's home folder.
pub fn create() {
    if let Err(err) = disk::create_dir_all(HOME) {
        log::error!("demo home: {err}");
    }
    disk::set_remote_loader(fetch);
    let manifest = match fetch(&format!("{FILES}/manifest.txt")) {
        Ok(manifest) => String::from_utf8_lossy(&manifest).into_owned(),
        Err(err) => {
            log::error!("no demo files (made by web/demo-files.sh): {err}");
            return;
        }
    };
    for line in manifest.lines() {
        if let Err(err) = add_line(line) {
            log::error!("demo file, {line}: {err}");
        }
    }
}

/// Adds a line of the manifest, one of:
///   `<size><TAB><age in seconds><TAB><path>`
///   `l<TAB><target><TAB><path>`
fn add_line(line: &str) -> io::Result<()> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "bad line");
    let mut parts = line.splitn(3, '\t');
    let (first, second, path) = (
        parts.next().ok_or_else(invalid)?,
        parts.next().ok_or_else(invalid)?,
        parts.next().ok_or_else(invalid)?,
    );
    let target = Path::new(HOME).join(path);
    if let Some(parent) = target.parent() {
        disk::create_dir_all(parent)?;
    }
    if first == "l" {
        return disk::symlink(second, target);
    }
    let len = first.parse().map_err(|_| invalid())?;
    let age = Duration::from_secs(second.parse().map_err(|_| invalid())?);
    let url: Vec<String> = path
        .split('/')
        .map(|part| String::from(js_sys::encode_uri_component(part)))
        .collect();
    disk::add_remote(
        target,
        format!("{FILES}/{}", url.join("/")),
        len,
        disk::now() - age,
    )
}

/// Gets a file from the server, and waits for it: the files' callers can't
/// wait any other way, so the page stops until it's here. Sync requests can't
/// ask for bytes, so it comes as text that maps each byte to one character.
fn fetch(url: &str) -> io::Result<Vec<u8>> {
    let failed = |err: wasm_bindgen::JsValue| io::Error::other(format!("{err:?}"));
    let request = web_sys::XmlHttpRequest::new().map_err(failed)?;
    request.open_with_async("GET", url, false).map_err(failed)?;
    request
        .override_mime_type("text/plain; charset=x-user-defined")
        .map_err(failed)?;
    request.send().map_err(failed)?;
    let status = request.status().map_err(failed)?;
    if status != 200 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{url}: HTTP {status}"),
        ));
    }
    let text = request.response_text().map_err(failed)?.unwrap_or_default();
    Ok(text.encode_utf16().map(|unit| unit as u8).collect())
}
