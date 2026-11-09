//! Files put into a document: what to write for them in the markdown, and
//! copying pictures to where the document can keep them.

use std::{
    io,
    path::{Component, Path, PathBuf},
};

/// Escapes what can't be in a link as it is. Other letters can, and stay
/// readable: a Japanese file name is still one in the source.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control()
            || matches!(
                c,
                ' ' | '(' | ')' | '<' | '>' | '[' | ']' | '%' | '#' | '?' | '"'
            )
        {
            let mut bytes = [0; 4];
            for byte in c.encode_utf8(&mut bytes).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Whether the file is a picture a document can show.
pub fn is_picture(path: &Path) -> bool {
    (crate::images::is_image(path) && !crate::raw::is_raw(path))
        || path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
}

/// The way from a document's folder to a file, as a link. A file on another
/// drive has no such way, so it gets a `file:` address.
pub fn link_to(dir: &Path, file: &Path) -> String {
    let parts = |path: &Path| -> Vec<String> {
        path.components()
            .filter_map(|part| match part {
                Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect()
    };
    let root = |path: &Path| {
        path.components()
            .next()
            .filter(|part| matches!(part, Component::Prefix(_)))
            .map(|part| part.as_os_str().to_owned())
    };
    if root(dir) != root(file) {
        let path = file.to_string_lossy().replace('\\', "/");
        return format!("file:///{}", encode(path.trim_start_matches('/')));
    }
    let (from, to) = (parts(dir), parts(file));
    let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut link: Vec<String> = vec!["..".to_owned(); from.len() - shared];
    link.extend(to[shared..].iter().map(|part| encode(part)));
    link.join("/")
}

/// Where a document keeps its pictures: `folder` in the document's own folder.
/// A name that climbs out isn't taken: settings shouldn't write elsewhere.
fn pictures_dir(dir: &Path, folder: &str) -> PathBuf {
    let folder = Path::new(folder.trim());
    if folder
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        dir.join(folder)
    } else {
        dir.to_path_buf()
    }
}

/// The markdown for files dropped on a document. Pictures from elsewhere are
/// copied in, so the document keeps working wherever it goes; anything else
/// is only linked to, as it may be big.
pub fn markdown_for(dir: &Path, folder: &str, files: &[PathBuf]) -> io::Result<String> {
    let mut lines = Vec::new();
    for file in files {
        let name = file
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        if is_picture(file) {
            let kept = if file.starts_with(dir) {
                file.clone()
            } else {
                let to = pictures_dir(dir, folder);
                disk::create_dir_all(&to)?;
                let path = crate::system_clipboard::free_name(&to, &name);
                disk::copy(file, &path)?;
                path
            };
            let alt = Path::new(&name)
                .file_stem()
                .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
            lines.push(format!("![{}]({})", escape(&alt), link_to(dir, &kept)));
        } else {
            lines.push(format!("[{}]({})", escape(&name), link_to(dir, file)));
        }
    }
    Ok(lines.join("\n\n"))
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

/// Saves a pasted picture, and returns the markdown for it.
pub fn save_pasted(
    dir: &Path,
    folder: &str,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> io::Result<String> {
    let to = pictures_dir(dir, folder);
    let path = crate::system_clipboard::save_picture(&to, width, height, rgba)?;
    Ok(format!("![pasted image]({})", link_to(dir, &path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn links_are_relative_and_encoded() {
        let dir = Path::new("/home/me/docs");
        let link = |file: &str| link_to(dir, Path::new(file));
        assert_eq!(link("/home/me/docs/a.png"), "a.png");
        assert_eq!(link("/home/me/docs/pics/a.png"), "pics/a.png");
        assert_eq!(link("/home/me/a.png"), "../a.png");
        assert_eq!(link("/etc/x/a.png"), "../../../etc/x/a.png");
        assert_eq!(
            link("/home/me/docs/my pic (1).png"),
            "my%20pic%20%281%29.png"
        );
        assert_eq!(link("/home/me/docs/日本語.png"), "日本語.png");
    }

    #[cfg(not(windows))]
    #[test]
    fn links_lead_back_to_the_file() {
        let dir = Path::new("/home/me/docs");
        for file in ["/home/me/docs/my pic (1)#2?.png", "/home/other/é.txt"] {
            let link = crate::markdown::parse::Link {
                url: link_to(dir, Path::new(file)),
                title: String::new(),
                wiki: false,
            };
            assert_eq!(
                crate::markdown::links::resolve(&link, dir),
                crate::markdown::Target::File(file.into()),
                "{}",
                link.url
            );
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("egui-files-assets-{name}-{}", std::process::id()));
        let _ = disk::remove_dir_all(&dir);
        disk::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pictures_from_elsewhere_are_copied_in() {
        let root = temp("copy");
        let (docs, other) = (root.join("docs"), root.join("other"));
        disk::create_dir_all(&docs).unwrap();
        disk::create_dir_all(&other).unwrap();
        let picture = other.join("a b.png");
        disk::write(&picture, "pixels").unwrap();

        let once = markdown_for(&docs, "", std::slice::from_ref(&picture)).unwrap();
        assert_eq!(once, "![a b](a%20b.png)");
        assert_eq!(disk::read(docs.join("a b.png")).unwrap(), b"pixels");
        assert!(disk::path_exists(&picture), "the original stays");

        let twice = markdown_for(&docs, "", std::slice::from_ref(&picture)).unwrap();
        assert_eq!(twice, "![a b](a%20b-1.png)", "nothing is overwritten");

        let kept = markdown_for(&docs, "assets", &[picture]).unwrap();
        assert_eq!(kept, "![a b](assets/a%20b.png)");
        assert!(disk::path_exists(docs.join("assets/a b.png")));
        let _ = disk::remove_dir_all(&root);
    }

    #[test]
    fn pictures_already_in_the_folder_stay_where_they_are() {
        let docs = temp("in-place");
        disk::create_dir_all(docs.join("pics")).unwrap();
        let picture = docs.join("pics/a.png");
        disk::write(&picture, "pixels").unwrap();
        assert_eq!(
            markdown_for(&docs, "assets", &[picture]).unwrap(),
            "![a](pics/a.png)"
        );
        assert!(!disk::path_exists(docs.join("assets")));
        let _ = disk::remove_dir_all(&docs);
    }

    #[test]
    fn other_files_are_linked_not_copied() {
        let root = temp("other");
        let docs = root.join("docs");
        disk::create_dir_all(&docs).unwrap();
        let file = root.join("big [1].zip");
        disk::write(&file, "bytes").unwrap();
        assert_eq!(
            markdown_for(&docs, "", &[file]).unwrap(),
            "[big \\[1\\].zip](../big%20%5B1%5D.zip)"
        );
        assert!(!disk::path_exists(docs.join("big [1].zip")));
        let _ = disk::remove_dir_all(&root);
    }

    #[test]
    fn several_files_each_get_a_paragraph() {
        let docs = temp("several");
        disk::write(docs.join("a.png"), "x").unwrap();
        disk::write(docs.join("b.svg"), "x").unwrap();
        let text = markdown_for(&docs, "", &[docs.join("a.png"), docs.join("b.svg")]).unwrap();
        assert_eq!(text, "![a](a.png)\n\n![b](b.svg)");
        let _ = disk::remove_dir_all(&docs);
    }

    #[test]
    fn the_pictures_folder_stays_inside_the_documents() {
        let dir = Path::new("/docs");
        assert_eq!(pictures_dir(dir, ""), dir);
        assert_eq!(pictures_dir(dir, "assets/img"), dir.join("assets/img"));
        assert_eq!(pictures_dir(dir, "../up"), dir);
        assert_eq!(pictures_dir(dir, "/etc"), dir);
    }

    #[test]
    fn pasted_pictures_are_saved_as_png() {
        let docs = temp("paste");
        let text = save_pasted(&docs, "assets", 1, 1, vec![255, 0, 0, 255]).unwrap();
        assert!(text.starts_with("![pasted image](assets/pasted-"), "{text}");
        let name = text.split("assets/").nth(1).unwrap().trim_end_matches(')');
        let saved = image::load_from_memory(&disk::read(docs.join("assets").join(name)).unwrap());
        assert_eq!(saved.unwrap().to_rgba8().as_raw(), &[255, 0, 0, 255]);
        assert!(
            save_pasted(&docs, "", 2, 2, vec![0; 3]).is_err(),
            "too few bytes"
        );
        let _ = disk::remove_dir_all(&docs);
    }
}
