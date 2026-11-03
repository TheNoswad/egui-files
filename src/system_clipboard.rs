//! What other apps put on the system's clipboard, beyond text: files copied
//! in a file manager, and pictures. egui only passes on text.

use std::{io, path::Path, path::PathBuf};

/// Something to paste. Android and the browser never have any.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(any(target_os = "android", target_arch = "wasm32"), allow(dead_code))]
pub enum Pasted {
    Files(Vec<PathBuf>),
    Picture {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
}

/// Files come first: a file manager's copy is of the file, whatever else it
/// may offer, and a picture's own format and name are kept that way.
#[cfg(all(not(test), not(any(target_os = "android", target_arch = "wasm32"))))]
pub fn read() -> Option<Pasted> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let files = clipboard.get().file_list().unwrap_or_default();
    let files: Vec<PathBuf> = files.into_iter().filter(|p| disk::path_exists(p)).collect();
    if !files.is_empty() {
        return Some(Pasted::Files(files));
    }
    let image = clipboard.get_image().ok()?;
    Some(Pasted::Picture {
        width: u32::try_from(image.width).ok()?,
        height: u32::try_from(image.height).ok()?,
        rgba: image.bytes.into_owned(),
    })
}

/// Android and the browser give no way to read it.
#[cfg(all(not(test), any(target_os = "android", target_arch = "wasm32")))]
pub fn read() -> Option<Pasted> {
    None
}

/// Tests get what they set, and never what's on the machine's real clipboard.
#[cfg(test)]
pub fn read() -> Option<Pasted> {
    TEST_CLIPBOARD.with(|clipboard| clipboard.borrow_mut().take())
}

#[cfg(test)]
thread_local! {
    pub static TEST_CLIPBOARD: std::cell::RefCell<Option<Pasted>> =
        const { std::cell::RefCell::new(None) };
}

/// Whether paste was pressed. egui-winit sends text as an event, and otherwise
/// the key press (see patches/README.md). On Windows, `file_clipboard::PasteKey`
/// reports that key as an event too, so it would count twice.
pub fn paste_pressed(input: &egui::InputState) -> bool {
    input.events.iter().any(|event| match event {
        egui::Event::Paste(_) => true,
        egui::Event::Key {
            key: egui::Key::V,
            pressed: true,
            repeat: false,
            modifiers,
            ..
        } => !cfg!(windows) && modifiers.command,
        _ => false,
    })
}

/// Saves a pasted picture as a new file in `dir`, and returns its path.
pub fn save_picture(dir: &Path, width: u32, height: u32, rgba: Vec<u8>) -> io::Result<PathBuf> {
    let image = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| io::Error::other("the picture on the clipboard is malformed"))?;
    let mut png = Vec::new();
    image
        .write_to(&mut io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(io::Error::other)?;
    disk::create_dir_all(dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = free_name(dir, &format!("pasted-{stamp}.png"));
    disk::write(&path, png)?;
    Ok(path)
}

/// A name that isn't taken in `dir`: `a.png`, then `a-1.png`, and so on.
pub fn free_name(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if !disk::path_exists(&path) {
        return path;
    }
    let as_path = Path::new(name);
    let stem = as_path
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let extension = as_path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (1..)
        .map(|n| dir.join(format!("{stem}-{n}{extension}")))
        .find(|path| !disk::path_exists(path))
        .expect("a free name, among so many")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pressed(events: Vec<egui::Event>) -> bool {
        let mut input = egui::InputState::default();
        input.events = events;
        paste_pressed(&input)
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers, repeat: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat,
            modifiers,
        }
    }

    #[test]
    fn paste_is_the_event_or_the_key() {
        assert!(pressed(vec![egui::Event::Paste("x".into())]));
        let ctrl_v = key(egui::Key::V, egui::Modifiers::COMMAND, false);
        assert_eq!(pressed(vec![ctrl_v]), !cfg!(windows));
        assert!(!pressed(vec![key(
            egui::Key::V,
            egui::Modifiers::NONE,
            false
        )]));
        assert!(!pressed(vec![key(
            egui::Key::V,
            egui::Modifiers::COMMAND,
            true
        )]));
        assert!(!pressed(vec![key(
            egui::Key::C,
            egui::Modifiers::COMMAND,
            false
        )]));
    }

    #[test]
    fn pictures_get_a_new_name_each() {
        let dir = std::env::temp_dir().join(format!("egui-files-paste-{}", std::process::id()));
        let _ = disk::remove_dir_all(&dir);
        let one = save_picture(&dir, 1, 1, vec![1, 2, 3, 255]).unwrap();
        let two = save_picture(&dir, 1, 1, vec![1, 2, 3, 255]).unwrap();
        assert_ne!(one, two);
        assert!(disk::path_exists(&one) && disk::path_exists(&two));
        assert!(save_picture(&dir, 2, 2, vec![0; 3]).is_err());
        let _ = disk::remove_dir_all(&dir);
    }
}
