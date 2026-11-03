//! Bringing files into the browser's in-memory filesystem: picked in a dialog
//! or dropped on the page. They stay in the browser; nothing is uploaded.
//!
//! The browser hands them over in events of its own, so they wait here for the
//! app to start a job with them on its next frame.

use std::{
    cell::RefCell,
    path::{Component, Path, PathBuf},
};

use js_sys::{Array, Promise, Uint8Array};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    DragEvent, File, FileSystemDirectoryEntry, FileSystemEntry, FileSystemFileEntry,
    HtmlInputElement,
};

use crate::jobs::{Paste, Progress, Transfer};

/// What one job takes in at most: it all goes into memory, which ends at 4 GB
/// for the whole page, and running out of it ends the page.
const MAX_BYTES: u64 = 1 << 30;
/// How much of a file is read at a time.
const CHUNK: u64 = 1 << 20;

/// Something brought in, at this path relative to where it's going.
pub struct Incoming {
    path: PathBuf,
    /// `None` for a folder.
    file: Option<File>,
}

thread_local! {
    static WAITING: RefCell<Vec<Vec<Incoming>>> = const { RefCell::new(Vec::new()) };
    static CONTEXT: RefCell<Option<egui::Context>> = const { RefCell::new(None) };
}

/// The batches brought in since the last call.
pub fn take() -> Vec<Vec<Incoming>> {
    WAITING.take()
}

fn arrive(items: Vec<Incoming>) {
    if items.is_empty() {
        return;
    }
    WAITING.with_borrow_mut(|waiting| waiting.push(items));
    CONTEXT.with_borrow(|ctx| {
        if let Some(ctx) = ctx {
            ctx.request_repaint();
        }
    });
}

/// Starts taking drops on the page, ahead of eframe: it reads only files, all
/// at once, and fails on folders.
pub fn install(ctx: &egui::Context) {
    CONTEXT.set(Some(ctx.clone()));
    let Some(window) = web_sys::window() else {
        return;
    };
    let on_drop = Closure::<dyn FnMut(DragEvent)>::new(on_drop);
    let added = window.add_event_listener_with_callback_and_bool(
        "drop",
        on_drop.as_ref().unchecked_ref(),
        true,
    );
    if added.is_err() {
        log::error!("couldn't listen for dropped files");
    }
    // For as long as the page is there.
    on_drop.forget();
}

fn on_drop(event: DragEvent) {
    let Some(transfer) = event.data_transfer() else {
        return;
    };
    event.prevent_default();
    event.stop_propagation();
    // eframe thinks the drag is still over the page, as it won't see the drop.
    if let Some(target) = event.target()
        && let Ok(leave) = web_sys::Event::new("dragleave")
    {
        let _ = target.dispatch_event(&leave);
    }
    // These last only as long as this event, unlike what comes of them.
    let items = transfer.items();
    let mut entries = Vec::new();
    let mut files = Vec::new();
    for i in 0..items.length() {
        let Some(item) = items.get(i) else { continue };
        match item.webkit_get_as_entry() {
            Ok(Some(entry)) => entries.push(entry),
            _ => files.extend(item.get_as_file().ok().flatten()),
        }
    }
    wasm_bindgen_futures::spawn_local(async move {
        let mut found: Vec<Incoming> = files
            .into_iter()
            .map(|file| Incoming {
                path: PathBuf::from(file.name()),
                file: Some(file),
            })
            .collect();
        for entry in entries {
            if let Err(err) = walk(entry, Path::new(""), &mut found).await {
                log::error!("couldn't read what was dropped: {err:?}");
            }
        }
        arrive(found);
    });
}

/// Adds an entry dropped on the page, and what's in it if it's a folder.
async fn walk(
    entry: FileSystemEntry,
    parent: &Path,
    found: &mut Vec<Incoming>,
) -> Result<(), JsValue> {
    let path = parent.join(entry.name());
    if entry.is_directory() {
        let dir: FileSystemDirectoryEntry = entry.unchecked_into();
        found.push(Incoming {
            path: path.clone(),
            file: None,
        });
        let reader = dir.create_reader();
        loop {
            // Comes in batches, until an empty one.
            let promise = Promise::new(&mut |resolve, reject| {
                let _ = reader.read_entries_with_callback_and_callback(&resolve, &reject);
            });
            let batch: Array = JsFuture::from(promise).await?.unchecked_into();
            if batch.length() == 0 {
                break;
            }
            for child in batch.iter() {
                Box::pin(walk(child.unchecked_into(), &path, found)).await?;
            }
        }
    } else {
        let entry: FileSystemFileEntry = entry.unchecked_into();
        let promise = Promise::new(&mut |resolve, reject| {
            entry.file_with_callback_and_callback(&resolve, &reject);
        });
        let file: File = JsFuture::from(promise).await?.unchecked_into();
        found.push(Incoming {
            path,
            file: Some(file),
        });
    }
    Ok(())
}

/// Opens the browser's dialog for choosing files, or a folder.
pub fn pick(folder: bool) {
    if let Err(err) = open_picker(folder) {
        log::error!("couldn't open a file dialog: {err:?}");
    }
}

fn open_picker(folder: bool) -> Result<(), JsValue> {
    let document = web_sys::window()
        .and_then(|window| window.document())
        .ok_or("no document")?;
    let input: HtmlInputElement = document.create_element("input")?.unchecked_into();
    input.set_type("file");
    input.set_multiple(true);
    if folder {
        input.set_webkitdirectory(true);
    }
    // Not shown, but in the page: not all browsers report a choice otherwise.
    input.style().set_property("display", "none")?;
    document.body().ok_or("no body")?.append_child(&input)?;
    let chosen = input.clone();
    let on_change = Closure::once_into_js(move || {
        let mut found = Vec::new();
        if let Some(files) = chosen.files() {
            for i in 0..files.length() {
                let Some(file) = files.get(i) else { continue };
                // Includes the folder chosen, for a folder.
                let relative = js_sys::Reflect::get(&file, &"webkitRelativePath".into())
                    .ok()
                    .and_then(|path| path.as_string())
                    .filter(|path| !path.is_empty());
                let path = relative.unwrap_or_else(|| file.name());
                found.push(Incoming {
                    path: PathBuf::from(path),
                    file: Some(file),
                });
            }
        }
        chosen.remove();
        arrive(found);
    });
    input.add_event_listener_with_callback("change", on_change.unchecked_ref())?;
    input.click();
    Ok(())
}

/// A path to create under the destination, if it stays under it.
fn safe(path: &Path) -> Option<PathBuf> {
    let safe = path.components().all(|c| matches!(c, Component::Normal(_)));
    (safe && path.components().next().is_some()).then(|| path.to_path_buf())
}

/// The job: reads everything into a folder beside where it's going, then moves
/// it in place, asking about names already taken, as a move would.
pub(crate) async fn import(items: Vec<Incoming>, dest: &Path, p: &Progress) {
    let bytes: u64 = items
        .iter()
        .filter_map(|item| item.file.as_ref())
        .map(|file| file.size() as u64)
        .sum();
    if bytes > MAX_BYTES {
        return p.error(
            dest,
            format!(
                "that's {}, and the browser can only hold {} of files at a time",
                crate::fs::human_size(bytes),
                crate::fs::human_size(MAX_BYTES),
            ),
        );
    }
    let files = items.iter().filter(|item| item.file.is_some()).count();
    p.add_totals(bytes, files as u64);
    let staging = crate::fs::unique_path(dest, ".adding".as_ref());
    if let Err(err) = disk::create_dir_all(&staging) {
        return p.error(&staging, err);
    }
    let mut tops = Vec::new();
    for item in items {
        if p.cancelled() {
            break;
        }
        let Some(relative) = safe(&item.path) else {
            p.error(&item.path, "skipped: it would be outside the folder");
            continue;
        };
        let target = staging.join(&relative);
        let made = match &item.file {
            Some(file) => read_file(file, &target, p).await,
            None => disk::create_dir_all(&target).map_err(|err| err.to_string()),
        };
        if let Err(err) = made {
            let _ = disk::remove_file(&target);
            if !p.cancelled() {
                p.error(&relative, err);
            }
        }
        let top = relative
            .components()
            .next()
            .map(|c| c.as_os_str().to_owned());
        if let Some(top) = top
            && !tops.contains(&top)
        {
            tops.push(top);
        }
    }
    if !p.cancelled() {
        let mut paste = Paste::new(Transfer::Move, p);
        for top in &tops {
            let source = staging.join(top);
            if disk::symlink_metadata(&source).is_ok()
                && !paste.place(&source, &dest.join(top)).await
            {
                break;
            }
        }
        paste.finish_moves().await;
    }
    let _ = disk::remove_dir_all(&staging);
}

async fn read_file(file: &File, target: &Path, p: &Progress) -> Result<(), String> {
    use std::io::Write as _;
    p.set_current(target);
    if let Some(parent) = target.parent() {
        disk::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let mut out = disk::File::create_new(target).map_err(|err| err.to_string())?;
    let size = file.size() as u64;
    // All of it up front, not a copy of it each time it grows.
    out.set_len(size).map_err(|err| err.to_string())?;
    let mut start = 0;
    while start < size {
        if p.cancelled() {
            return Err("cancelled".to_owned());
        }
        let end = (start + CHUNK).min(size);
        let chunk = file
            .slice_with_f64_and_f64(start as f64, end as f64)
            .map_err(js_error)?;
        let buffer = JsFuture::from(chunk.array_buffer())
            .await
            .map_err(js_error)?;
        let bytes = Uint8Array::new(&buffer).to_vec();
        out.write_all(&bytes).map_err(|err| err.to_string())?;
        p.add_done(bytes.len() as u64, 0);
        start = end;
    }
    p.add_done(0, 1);
    let modified =
        std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(file.last_modified() / 1000.0);
    out.set_modified(modified).map_err(|err| err.to_string())
}

fn js_error(err: JsValue) -> String {
    err.as_string().unwrap_or_else(|| format!("{err:?}"))
}
