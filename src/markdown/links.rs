//! Where a link in a document goes.

use std::path::{Component, Path, PathBuf};

use percent_encoding::percent_decode_str;

use super::parse::Link;

/// How many files a wikilink's search looks at before giving up.
const WIKI_SEARCH_LIMIT: usize = 20_000;

/// Other documents' names, tried in this order.
pub const EXTENSIONS: &[&str] = &["md", "markdown", "mdown", "mkd", "mkdn", "mdwn", "mdx"];

/// Whether this file is a markdown document.
pub fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Opens in the browser or mail app.
    Web(String),
    /// A heading or footnote in this document.
    Anchor(String),
    /// Another document, and a heading in it.
    Document(PathBuf, Option<String>),
    /// Anything else on disk.
    File(PathBuf),
    /// A link that goes nowhere we'd follow, such as `javascript:`.
    Refused,
}

/// Works out where a link goes, for a document in `dir`.
pub fn resolve(link: &Link, dir: &Path) -> Target {
    let url = link.url.trim();
    if let Some(anchor) = url.strip_prefix('#') {
        return Target::Anchor(decode(anchor));
    }
    if link.wiki {
        return wiki(url, dir);
    }
    if let Some(scheme) = scheme(url) {
        return match scheme.to_ascii_lowercase().as_str() {
            "http" | "https" | "mailto" => Target::Web(url.to_owned()),
            "file" => local(url, dir),
            _ => Target::Refused,
        };
    }
    local(url, dir)
}

/// The scheme of a URL. Windows' `C:\x` and `C:/x` aren't URLs.
fn scheme(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once(':')?;
    let valid = scheme.len() > 1
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    (valid && !rest.is_empty()).then_some(scheme)
}

fn local(url: &str, dir: &Path) -> Target {
    let Some((path, anchor)) = local_path(url, dir) else {
        return Target::Refused;
    };
    if super::is_markdown(&path) {
        Target::Document(path, anchor)
    } else {
        Target::File(path)
    }
}

/// The file a relative link or `file:` URL names, and its `#fragment`.
pub fn local_path(url: &str, dir: &Path) -> Option<(PathBuf, Option<String>)> {
    let url = url.trim();
    let url = url.strip_prefix("file://").unwrap_or(url);
    let (path, anchor) = match url.split_once('#') {
        Some((path, anchor)) => (path, Some(decode(anchor))),
        None => (url, None),
    };
    let path = path.split('?').next().unwrap_or(path);
    if path.is_empty() {
        return None;
    }
    let decoded = decode(path);
    // Windows' `file:///C:/x`.
    #[cfg(windows)]
    let decoded = decoded
        .strip_prefix('/')
        .filter(|rest| rest.as_bytes().get(1) == Some(&b':'))
        .map_or(decoded.clone(), str::to_owned);
    Some((normalize(&dir.join(&decoded)), anchor))
}

fn decode(text: &str) -> String {
    percent_decode_str(text).decode_utf8_lossy().into_owned()
}

/// Takes out `.` and `..`, without asking the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            part => out.push(part),
        }
    }
    out
}

/// `[[Name]]` or `[[folder/Name#Heading]]`: beside the document, or else
/// anywhere in its vault or repository.
fn wiki(url: &str, dir: &Path) -> Target {
    let (name, anchor) = match url.split_once('#') {
        Some((name, anchor)) => (name, Some(super::parse::slug(anchor))),
        None => (url, None),
    };
    if name.is_empty() {
        return anchor.map_or(Target::Refused, Target::Anchor);
    }
    let file_name = if super::is_markdown(Path::new(name)) || Path::new(name).extension().is_some()
    {
        name.to_owned()
    } else {
        format!("{name}.md")
    };
    let beside = normalize(&dir.join(&file_name));
    let path = if disk::metadata(&beside).is_ok() {
        beside
    } else {
        search(&vault_root(dir), &file_name).unwrap_or(beside)
    };
    if super::is_markdown(&path) {
        Target::Document(path, anchor)
    } else {
        Target::File(path)
    }
}

/// The folder notes are linked within: the nearest one with `.obsidian` or
/// `.git`, or else the document's own.
fn vault_root(dir: &Path) -> PathBuf {
    dir.ancestors()
        .find(|folder| {
            disk::metadata(folder.join(".obsidian")).is_ok()
                || disk::metadata(folder.join(".git")).is_ok()
        })
        .unwrap_or(dir)
        .to_path_buf()
}

/// Finds the file whose path ends in `name`, breadth first, so the nearest
/// wins, skipping hidden folders.
fn search(root: &Path, name: &str) -> Option<PathBuf> {
    let suffix = Path::new(name);
    let mut folders = std::collections::VecDeque::from([root.to_path_buf()]);
    let mut seen = 0;
    while let Some(folder) = folders.pop_front() {
        let Ok(entries) = disk::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > WIKI_SEARCH_LIMIT {
                return None;
            }
            let path = entry.path();
            let hidden = entry.file_name().to_string_lossy().starts_with('.');
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                if !hidden {
                    folders.push_back(path);
                }
            } else if path.ends_with(suffix) {
                return Some(path);
            }
        }
    }
    None
}

/// Opens a web link or mail address where the system does that.
pub fn open_web(ctx: &egui::Context, url: String) {
    #[cfg(target_os = "android")]
    {
        // egui's own way needs a crate that isn't in the Android build.
        let _ = ctx;
        if let Err(err) = crate::android::open_url(&url) {
            eprintln!("Could not open {url}: {err}");
        }
    }
    #[cfg(not(target_os = "android"))]
    ctx.open_url(egui::OpenUrl::new_tab(url));
}

/// What came of opening a file a link names.
pub enum Opened {
    Done,
    Missing,
    /// Shown in its folder rather than run: a document shouldn't be able to
    /// start a program with a single click.
    Reveal,
    Archive,
    Failed(std::io::Error),
}

/// Opens a file with its app, as the file list would, unless it's a program.
pub fn open_local(path: &Path) -> Opened {
    let Ok(meta) = disk::metadata(path) else {
        return Opened::Missing;
    };
    if meta.is_dir() || is_program(path, &meta) {
        Opened::Reveal
    } else if crate::archive::is_archive(path) {
        Opened::Archive
    } else {
        match crate::fs::open(path) {
            Ok(()) => Opened::Done,
            Err(err) => Opened::Failed(err),
        }
    }
}

/// Whether opening the file would run something.
pub fn is_program(path: &Path, meta: &disk::Metadata) -> bool {
    const RUN_WHEN_OPENED: &[&str] = &[
        "app",
        "application",
        "appimage",
        "bat",
        "cmd",
        "com",
        "cpl",
        "desktop",
        "exe",
        "hta",
        "jar",
        "js",
        "jse",
        "lnk",
        "msc",
        "msi",
        "msp",
        "pif",
        "ps1",
        "reg",
        "scr",
        "sh",
        "url",
        "vb",
        "vbe",
        "vbs",
        "wsf",
        "wsh",
    ];
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    crate::fs::is_executable(Some(meta)) || RUN_WHEN_OPENED.contains(&extension.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(url: &str) -> Link {
        Link {
            url: url.to_owned(),
            title: String::new(),
            wiki: false,
        }
    }

    #[test]
    fn links_are_told_apart() {
        let dir = Path::new("/docs/a");
        let go = |url: &str| resolve(&link(url), dir);
        assert_eq!(
            go("https://example.com/x"),
            Target::Web("https://example.com/x".to_owned())
        );
        assert_eq!(go("mailto:a@b.c"), Target::Web("mailto:a@b.c".to_owned()));
        assert_eq!(
            go("#some-heading"),
            Target::Anchor("some-heading".to_owned())
        );
        assert_eq!(
            go("other.md#part"),
            Target::Document("/docs/a/other.md".into(), Some("part".to_owned()))
        );
        assert_eq!(
            go("../b/My%20Note.MD"),
            Target::Document("/docs/b/My Note.MD".into(), None)
        );
        assert_eq!(go("pic.png"), Target::File("/docs/a/pic.png".into()));
        assert_eq!(go("/abs/x.txt"), Target::File("/abs/x.txt".into()));
        assert_eq!(
            go("file:///abs/y.md"),
            Target::Document("/abs/y.md".into(), None)
        );
    }

    #[test]
    fn links_that_could_do_harm_are_refused() {
        let dir = Path::new("/docs");
        for url in [
            "javascript:alert(1)",
            "JAVASCRIPT:alert(1)",
            "vbscript:x",
            "data:text/html,x",
            "steam://run/1",
            "ms-msdt:/id",
            "",
        ] {
            assert_eq!(resolve(&link(url), dir), Target::Refused, "{url}");
        }
    }

    #[test]
    fn programs_are_told_apart() {
        let dir = std::env::temp_dir().join(format!("egui-files-links-{}", std::process::id()));
        let _ = disk::remove_dir_all(&dir);
        disk::create_dir_all(&dir).unwrap();
        for (name, program) in [
            ("a.txt", false),
            ("a.PDF", false),
            ("setup.EXE", true),
            ("run.bat", true),
            ("launcher.desktop", true),
        ] {
            let path = dir.join(name);
            disk::write(&path, "x").unwrap();
            let meta = disk::metadata(&path).unwrap();
            assert_eq!(is_program(&path, &meta), program, "{name}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join("tool");
            disk::write(&path, "x").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(is_program(&path, &disk::metadata(&path).unwrap()));
        }
        let _ = disk::remove_dir_all(&dir);
    }

    #[test]
    fn wikilinks_search_the_vault() {
        let root = std::env::temp_dir().join(format!("egui-files-wiki-{}", std::process::id()));
        let _ = disk::remove_dir_all(&root);
        disk::create_dir_all(root.join(".obsidian")).unwrap();
        disk::create_dir_all(root.join("notes/deep")).unwrap();
        disk::create_dir_all(root.join(".hidden")).unwrap();
        disk::write(root.join("notes/deep/Target.md"), "").unwrap();
        disk::write(root.join("notes/Beside.md"), "").unwrap();
        disk::write(root.join(".hidden/Secret.md"), "").unwrap();
        let dir = root.join("notes");
        let wiki_link = |url: &str| {
            resolve(
                &Link {
                    url: url.to_owned(),
                    title: String::new(),
                    wiki: true,
                },
                &dir,
            )
        };
        assert_eq!(
            wiki_link("Beside"),
            Target::Document(dir.join("Beside.md"), None)
        );
        assert_eq!(
            wiki_link("Target#A Heading"),
            Target::Document(dir.join("deep/Target.md"), Some("a-heading".to_owned()))
        );
        assert_eq!(
            wiki_link("Secret"),
            Target::Document(dir.join("Secret.md"), None),
            "not found in hidden folders, so it stays where it'd be created"
        );
        let _ = disk::remove_dir_all(&root);
    }
}
