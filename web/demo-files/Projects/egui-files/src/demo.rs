//! The web demo's files: a home folder, from `web/demo-files/` on the server.
//!
//! `web/demo-files.sh` puts them beside the page with a manifest, which is
//! read at startup, so they're listed right away. Each is fetched the first
//! time something opens it. Nothing is kept: a reload starts over.

use std::{io, path::Path, time::Duration};

pub const HOME: &str = "/home/demo";

/// Beside the page: where web/demo-files.sh puts them.
const FILES: &str = "files";

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
