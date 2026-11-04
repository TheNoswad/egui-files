//! Keeping links working when files move in the app.
//!
//! After a move, the documents around it are looked through for links that now
//! point the wrong way, and the user is shown what would change. Only the bytes
//! of a link's destination change, and a link that was already broken is left
//! as it is: it may mean something the moved files know nothing of.
//!
//! What's looked through: the whole of the git repository or Obsidian vault on
//! either side of the move, and without one, only the folders moved from and
//! to, and what moved. Walking all of a home folder after each rename would be
//! a lot to do unasked.
//!
//! What moves outside the app can't be told from a `git checkout`, so that's
//! left to [`repair`], asked for by the user: it finds what's broken, and what
//! it can by the file's name, where there's one file of that name.

use std::{
    cell::OnceCell,
    collections::{BTreeSet, HashMap},
    ops::Range,
    path::{Component, Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
};

use pulldown_cmark::{Event, LinkType, Parser, Tag};

use super::{links, parse};

/// How many files and folders a scan looks at before giving up on the rest.
const SCAN_LIMIT: usize = 20_000;
/// A document larger than this isn't read, and that's said.
const MAX_DOCUMENT: u64 = 64 << 20;
/// Folders of tools' own, where nothing's written to be read.
const TOOL_FOLDERS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".obsidian",
    ".trash",
    ".Trash",
    ".cache",
    ".venv",
    "node_modules",
    "__pycache__",
];

/// Something that moved, or was renamed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moved {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// A destination to change, in a document's source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkEdit {
    pub range: Range<usize>,
    pub old: String,
    pub new: String,
}

/// What's on the disk, as far as links are concerned.
pub struct World<'a> {
    pub exists: &'a dyn Fn(&Path) -> bool,
    /// The nearest file of this name to the top of the vault.
    pub nearest: &'a dyn Fn(&str) -> Option<PathBuf>,
}

/// Where something is now, if a move took it there.
pub fn moved_to(moves: &[Moved], path: &Path) -> Option<PathBuf> {
    moves.iter().find_map(|moved| {
        let rest = path.strip_prefix(&moved.from).ok()?;
        Some(if rest.as_os_str().is_empty() {
            moved.to.clone()
        } else {
            moved.to.join(rest)
        })
    })
}

/// Where something was, if a move took it from there.
fn moved_from(moves: &[Moved], path: &Path) -> Option<PathBuf> {
    moves.iter().find_map(|moved| {
        let rest = path.strip_prefix(&moved.to).ok()?;
        Some(if rest.as_os_str().is_empty() {
            moved.from.clone()
        } else {
            moved.from.join(rest)
        })
    })
}

/// The way from a folder to a path, with `/` on every system.
fn relative(from: &Path, to: &Path) -> Option<String> {
    let from: Vec<Component<'_>> = from.components().collect();
    let to: Vec<Component<'_>> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    // Different drives have no way between them.
    if common == 0 && (from.first() != to.first()) {
        return None;
    }
    let mut parts: Vec<String> = vec!["..".to_owned(); from.len() - common];
    for part in &to[common..] {
        parts.push(part.as_os_str().to_string_lossy().into_owned());
    }
    Some(parts.join("/"))
}

/// A path as a link's destination, where spaces and the like can't be as they are.
fn encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '%' => out.push_str("%25"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '\\' => out.push_str("%5C"),
            '"' => out.push_str("%22"),
            c => out.push(c),
        }
    }
    out
}

/// A link's destination in the source, as written.
struct Found {
    range: Range<usize>,
    /// What the parser made of it.
    url: String,
    wiki: bool,
    /// Written in `<angle brackets>`, which can hold spaces as they are.
    angled: bool,
}

/// The destination of `[text](destination "title")`, which is at its end.
fn inline_destination(source: &str, link: &Range<usize>) -> Option<(Range<usize>, bool)> {
    let text = source.get(link.clone())?;
    let close = text.rfind(')')?;
    // Back to the parenthesis that opens this one, that a `]` comes right before.
    let bytes = text.as_bytes();
    let mut depth = 0;
    let mut open = None;
    for at in (0..close).rev() {
        match bytes[at] {
            b')' if at == 0 || bytes[at - 1] != b'\\' => depth += 1,
            b'(' if at == 0 || bytes[at - 1] != b'\\' => {
                if depth == 0 {
                    open = Some(at);
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    let open = open?;
    if open == 0 || bytes[open - 1] != b']' {
        return None;
    }
    let inside = &text[open + 1..close];
    let skipped = inside.len() - inside.trim_start().len();
    let rest = &inside[skipped..];
    let start = link.start + open + 1 + skipped;
    if let Some(angled) = rest.strip_prefix('<') {
        let end = angled.find('>')?;
        return Some((start + 1..start + 1 + end, true));
    }
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some((start..start + end, false))
}

/// The link destinations of a document. Anything in code isn't a link, and
/// what can't be found exactly is passed over rather than guessed at.
fn destinations(source: &str) -> Vec<Found> {
    let mut found = Vec::new();
    let parser = Parser::new_ext(source, parse::options());
    for (_, definition) in parser.reference_definitions().iter() {
        let Some(text) = source.get(definition.span.clone()) else {
            continue;
        };
        // `[label]: destination "title"`
        let Some(colon) = text.find("]:") else {
            continue;
        };
        let after = &text[colon + 2..];
        let skipped = after.len() - after.trim_start().len();
        let rest = &after[skipped..];
        let start = definition.span.start + colon + 2 + skipped;
        let (range, angled) = match rest.strip_prefix('<') {
            Some(inner) => match inner.find('>') {
                Some(end) => (start + 1..start + 1 + end, true),
                None => continue,
            },
            None => {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                (start..start + end, false)
            }
        };
        found.push(Found {
            range,
            url: definition.dest.to_string(),
            wiki: false,
            angled,
        });
    }
    let mut in_comment = false;
    for (event, range) in parser.into_offset_iter() {
        if matches!(event, Event::Html(_) | Event::InlineHtml(_)) {
            html_destinations(source, range, &mut in_comment, &mut found);
            continue;
        }
        let (Event::Start(Tag::Link {
            link_type,
            dest_url,
            ..
        })
        | Event::Start(Tag::Image {
            link_type,
            dest_url,
            ..
        })) = event
        else {
            continue;
        };
        match link_type {
            LinkType::Inline => {
                let Some((dest, angled)) = inline_destination(source, &range) else {
                    continue;
                };
                found.push(Found {
                    range: dest,
                    url: dest_url.to_string(),
                    wiki: false,
                    angled,
                });
            }
            LinkType::WikiLink { .. } => {
                let Some(text) = source.get(range.clone()) else {
                    continue;
                };
                let Some(open) = text.find("[[") else {
                    continue;
                };
                let start = range.start + open + 2;
                let inner = &text[open + 2..];
                let end = inner.find(['|', ']']).unwrap_or(inner.len());
                // Its heading stays as it is.
                let end = inner[..end].find('#').unwrap_or(end);
                found.push(Found {
                    range: start..start + end,
                    url: inner[..end].to_owned(),
                    wiki: true,
                    angled: false,
                });
            }
            _ => {}
        }
    }
    found.sort_by_key(|found| found.range.start);
    found
}

/// The `src` and `href` of the tags in a piece of raw HTML. A comment isn't
/// code that runs, and can go on over several pieces.
fn html_destinations(
    source: &str,
    range: Range<usize>,
    in_comment: &mut bool,
    found: &mut Vec<Found>,
) {
    let Some(text) = source.get(range.clone()) else {
        return;
    };
    let mut at = 0;
    while at < text.len() {
        if *in_comment {
            match text[at..].find("-->") {
                Some(end) => {
                    at += end + 3;
                    *in_comment = false;
                }
                None => return,
            }
            continue;
        }
        let Some(open) = text[at..].find('<') else {
            return;
        };
        at += open;
        if text[at..].starts_with("<!--") {
            *in_comment = true;
            at += 4;
            continue;
        }
        let end = text[at..].find('>').map_or(text.len(), |end| at + end);
        attributes(text, at..end, range.start, found);
        at = end + 1;
    }
}

/// Finds the destinations among a tag's attributes.
fn attributes(text: &str, tag: Range<usize>, offset: usize, found: &mut Vec<Found>) {
    let lower = text[tag.clone()].to_ascii_lowercase();
    for name in ["src", "href"] {
        let mut from = 0;
        while let Some(hit) = lower[from..].find(name) {
            let start = from + hit;
            from = start + name.len();
            // The whole of the attribute's name.
            let before = lower[..start].chars().next_back();
            if !before.is_some_and(char::is_whitespace) {
                continue;
            }
            let rest = &lower[from..];
            let Some(value) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            let value = value.trim_start();
            let Some(quote) = value.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
                continue;
            };
            let begin = tag.start + from + (rest.len() - value.len()) + 1;
            let Some(len) = text[begin..tag.end].find(quote) else {
                continue;
            };
            found.push(Found {
                range: offset + begin..offset + begin + len,
                url: text[begin..begin + len].to_owned(),
                wiki: false,
                angled: false,
            });
        }
    }
}

/// What a plain link's destination should be now.
fn new_destination(
    found: &Found,
    written: &str,
    old_dir: &Path,
    new_dir: &Path,
    moves: &[Moved],
    world: &World<'_>,
) -> Option<String> {
    let url = found.url.trim();
    let is_file_url = url.starts_with("file://");
    let has_scheme = url.split_once(':').is_some_and(|(scheme, rest)| {
        scheme.len() > 1
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-')
            && !rest.starts_with('\\')
    });
    if url.is_empty() || url.starts_with('#') || (has_scheme && !is_file_url) {
        return None;
    }
    let (old_target, _) = links::local_path(url, old_dir)?;
    let target = moved_to(moves, &old_target).unwrap_or(old_target);
    // It wasn't there before: not ours to change.
    if !(world.exists)(&target) {
        return None;
    }
    // Still right, as when what links and what it links to moved together.
    if links::local_path(url, new_dir).is_some_and(|(now, _)| now == target) {
        return None;
    }
    written_as(found, written, url, new_dir, &target)
}

/// A destination for this target, written the way the old one was.
fn written_as(
    found: &Found,
    written: &str,
    url: &str,
    new_dir: &Path,
    target: &Path,
) -> Option<String> {
    let suffix = url.find(['#', '?']).map_or("", |at| &url[at..]);
    let is_file_url = url.starts_with("file://");
    let path = url.strip_prefix("file://").unwrap_or(url);
    let path_only = &path[..path.find(['#', '?']).unwrap_or(path.len())];
    let plain = if Path::new(&links::decoded(path_only)).is_absolute() {
        target.to_string_lossy().replace('\\', "/")
    } else {
        relative(new_dir, target)?
    };
    let plain = if written.starts_with("./") && !plain.starts_with("..") {
        format!("./{plain}")
    } else {
        plain
    };
    let scheme = if is_file_url { "file://" } else { "" };
    let path = if found.angled { plain } else { encode(&plain) };
    Some(format!("{scheme}{path}{suffix}"))
}

/// The file a `[[name]]` goes to, as the app follows it (see `links::wiki`): the
/// one beside the document, or else the nearest to the top of the vault.
fn wiki_goes_to(file: &str, dir: &Path, world: &World<'_>) -> Option<PathBuf> {
    let beside = dir.join(file);
    if (world.exists)(&beside) {
        return Some(beside);
    }
    (world.nearest)(file)
}

/// How far down from the top a path is, to tell which is nearest.
fn depth(path: &Path) -> usize {
    path.components().count()
}

/// What a `[[wikilink]]` should be now. One that's only a name goes on finding
/// its note wherever it moves to, until the note's name changes, or another of
/// that name would be found first: then it needs a path. One with a path in it
/// is relative to the vault, or else to the document.
fn new_wiki(
    name: &str,
    old_dir: &Path,
    new_dir: &Path,
    vault: &Path,
    moves: &[Moved],
    world: &World<'_>,
) -> Option<String> {
    let has_extension = Path::new(name).extension().is_some();
    let as_written = |path: &Path| -> Option<String> {
        let part = if has_extension {
            path.file_name()
        } else {
            path.file_stem()
        }?;
        Some(part.to_string_lossy().into_owned())
    };
    let file = if has_extension {
        name.to_owned()
    } else {
        format!("{name}.md")
    };
    if name.contains('/') {
        return [(vault, vault), (old_dir, new_dir)]
            .into_iter()
            .find_map(|(old_base, new_base)| {
                let (old, _) = links::local_path(&encode(&file), old_base)?;
                let target = moved_to(moves, &old).unwrap_or(old);
                if !(world.exists)(&target) {
                    return None;
                }
                let way = relative(new_base, &target)?;
                Some(if has_extension {
                    way
                } else {
                    way.strip_suffix(".md").unwrap_or(&way).to_owned()
                })
            })
            .filter(|way| way != name);
    }
    // Which it meant before the move: the one beside it, if there was one.
    let old_beside = old_dir.join(&file);
    let beside_now = moved_to(moves, &old_beside).unwrap_or_else(|| old_beside.clone());
    let meant = if (world.exists)(&beside_now) {
        old_beside
    } else {
        // Or the nearest of those there were. A tie can't be told apart, and
        // isn't the moved one's to win.
        let staying = (world.nearest)(&file).map(|now| moved_from(moves, &now).unwrap_or(now));
        let moving = moves
            .iter()
            .filter(|moved| moved.from.file_name().and_then(|n| n.to_str()) == Some(file.as_str()))
            .map(|moved| moved.from.clone())
            .min_by_key(|path| depth(path));
        match (staying, moving) {
            (Some(staying), Some(moving)) if depth(&moving) < depth(&staying) => moving,
            (Some(staying), _) => staying,
            (None, Some(moving)) => moving,
            (None, None) => return None,
        }
    };
    let target = moved_to(moves, &meant).unwrap_or(meant);
    if !(world.exists)(&target) {
        return None;
    }
    // By its name, if that gets there.
    let plain = as_written(&target)?;
    let plain_file = target.file_name()?.to_str()?;
    if wiki_goes_to(plain_file, new_dir, world).as_deref() == Some(target.as_path()) {
        return (plain != name).then_some(plain);
    }
    // Otherwise from the top of the vault.
    let way = relative(vault, &target)?;
    Some(if has_extension {
        way
    } else {
        way.strip_suffix(".md").unwrap_or(&way).to_owned()
    })
}

/// The changes to a document's links, for a document now at `document`.
pub fn edits_for(
    source: &str,
    document: &Path,
    vault: &Path,
    moves: &[Moved],
    world: &World<'_>,
) -> Vec<LinkEdit> {
    let new_dir = document.parent().unwrap_or(Path::new(""));
    let old_document = moved_from(moves, document);
    let old_document = old_document.as_deref().unwrap_or(document);
    let old_dir = old_document.parent().unwrap_or(Path::new(""));
    destinations(source)
        .into_iter()
        .filter_map(|found| {
            let written = source.get(found.range.clone())?;
            let new = if found.wiki {
                new_wiki(&found.url, old_dir, new_dir, vault, moves, world)
            } else {
                new_destination(&found, written, old_dir, new_dir, moves, world)
            }?;
            (new != written).then(|| LinkEdit {
                range: found.range.clone(),
                old: written.to_owned(),
                new,
            })
        })
        .collect()
}

/// The source with these changes made. Nothing else in it changes.
pub fn apply_edits(source: &str, edits: &[LinkEdit]) -> String {
    let mut out = source.to_owned();
    for edit in edits.iter().rev() {
        out.replace_range(edit.range.clone(), &edit.new);
    }
    out
}

/// A document with links to change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentChange {
    pub path: PathBuf,
    pub vault: PathBuf,
    pub edits: Vec<LinkEdit>,
}

/// What a move would change, or a repair.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Proposal {
    pub moves: Vec<Moved>,
    /// Asked for by the user, rather than following from a move.
    pub repair: Option<PathBuf>,
    pub documents: Vec<DocumentChange>,
    /// It stopped looking before seeing everything.
    pub incomplete: bool,
    /// What it couldn't read, and why.
    pub unread: Vec<(PathBuf, &'static str)>,
    /// Broken links that a repair can't work out: which, and in what.
    pub unresolved: Vec<(PathBuf, String)>,
}

fn exists(path: &Path) -> bool {
    disk::metadata(path).is_ok()
}

fn is_tool_folder(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| TOOL_FOLDERS.iter().any(|tool| name == *tool))
}

/// Files by name, to find them by. Built when first needed, as walking a vault
/// for each `[[link]]` would be a lot, and most documents have none.
struct Index {
    root: PathBuf,
    files: OnceCell<HashMap<String, Vec<PathBuf>>>,
}

impl Index {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            files: OnceCell::new(),
        }
    }

    fn files(&self) -> &HashMap<String, Vec<PathBuf>> {
        self.files.get_or_init(|| {
            let mut files: HashMap<String, Vec<PathBuf>> = HashMap::new();
            let mut folders = vec![self.root.clone()];
            let mut seen = 0;
            while let Some(folder) = folders.pop() {
                let Ok(entries) = disk::read_dir(&folder) else {
                    continue;
                };
                for entry in entries.flatten() {
                    seen += 1;
                    if seen > SCAN_LIMIT {
                        return files;
                    }
                    let path = entry.path();
                    let Ok(kind) = entry.file_type() else {
                        continue;
                    };
                    if kind.is_dir() {
                        if !is_tool_folder(&path) {
                            folders.push(path);
                        }
                    } else {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        files.entry(name).or_default().push(path);
                    }
                }
            }
            for found in files.values_mut() {
                found.sort_by_key(|path| (depth(path), path.clone()));
            }
            files
        })
    }

    fn nearest(&self, name: &str) -> Option<PathBuf> {
        self.files()
            .get(name)
            .and_then(|found| found.first())
            .cloned()
    }
}

/// Adds the documents in a folder, and in those within it if `deep`. False when
/// it stopped at the limit.
fn collect(folder: &Path, deep: bool, seen: &mut usize, documents: &mut BTreeSet<PathBuf>) -> bool {
    let mut folders = vec![folder.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let Ok(entries) = disk::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            *seen += 1;
            if *seen > SCAN_LIMIT {
                return false;
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if deep && !is_tool_folder(&path) {
                    folders.push(path);
                }
            } else if kind.is_file() && links::is_markdown(&path) {
                documents.insert(path);
            }
        }
    }
    true
}

/// Looks through the documents around these moves for links to change.
pub fn scan(moves: Vec<Moved>) -> Proposal {
    let mut documents = BTreeSet::new();
    let mut seen = 0;
    let mut complete = true;
    let mut deep = BTreeSet::new();
    let mut shallow = BTreeSet::new();
    for moved in &moves {
        for parent in [moved.from.parent(), moved.to.parent()]
            .into_iter()
            .flatten()
        {
            match links::marked_vault(parent) {
                Some(vault) => {
                    deep.insert(vault);
                }
                None => {
                    shallow.insert(parent.to_path_buf());
                }
            }
        }
        if links::is_markdown(&moved.to) {
            documents.insert(moved.to.clone());
        } else if disk::metadata(&moved.to).is_ok_and(|meta| meta.is_dir()) {
            deep.insert(moved.to.clone());
        }
    }
    for folder in &deep {
        complete &= collect(folder, true, &mut seen, &mut documents);
    }
    for folder in &shallow {
        complete &= collect(folder, false, &mut seen, &mut documents);
    }
    let mut out = Proposal {
        moves,
        incomplete: !complete,
        ..Proposal::default()
    };
    for path in documents {
        match changes_in(&path, &out.moves) {
            Ok(Some(change)) => out.documents.push(change),
            Ok(None) => {}
            Err(why) => out.unread.push((path, why)),
        }
    }
    out
}

/// Looks through the documents around this one for links that go nowhere, and
/// finds what it can by name.
pub fn repair(document: PathBuf) -> Proposal {
    let dir = document.parent().unwrap_or(Path::new("")).to_path_buf();
    let vault = links::marked_vault(&dir);
    let root = vault.clone().unwrap_or_else(|| dir.clone());
    let mut documents = BTreeSet::new();
    documents.insert(document.clone());
    let mut seen = 0;
    let complete = collect(&root, vault.is_some(), &mut seen, &mut documents);
    let index = Index::new(&root);
    let mut out = Proposal {
        repair: Some(document),
        incomplete: !complete,
        ..Proposal::default()
    };
    for path in documents {
        let source = match read_document(&path) {
            Ok(source) => source,
            Err(why) => {
                out.unread.push((path, why));
                continue;
            }
        };
        let (edits, unresolved) = repairs_for(&source, &path, &index);
        out.unresolved
            .extend(unresolved.into_iter().map(|url| (path.clone(), url)));
        if !edits.is_empty() {
            out.documents.push(DocumentChange {
                path,
                vault: root.clone(),
                edits,
            });
        }
    }
    out
}

/// The links here that go nowhere: the changes for those with one file of their
/// name, and those without.
fn repairs_for(source: &str, document: &Path, index: &Index) -> (Vec<LinkEdit>, Vec<String>) {
    let dir = document.parent().unwrap_or(Path::new(""));
    let (mut edits, mut unresolved) = (Vec::new(), Vec::new());
    for found in destinations(source) {
        let Some(written) = source.get(found.range.clone()) else {
            continue;
        };
        let url = found.url.trim();
        // A note goes by name, wherever it is, and finds its own way.
        if found.wiki {
            continue;
        }
        let local = !url.starts_with('#')
            && (url.starts_with("file://")
                || !url.split_once(':').is_some_and(|(scheme, rest)| {
                    scheme.len() > 1
                        && scheme
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-')
                        && !rest.starts_with('\\')
                }));
        if url.is_empty() || !local {
            continue;
        }
        let Some((target, _)) = links::local_path(url, dir) else {
            continue;
        };
        if exists(&target) {
            continue;
        }
        let name = target.file_name().map(|n| n.to_string_lossy().into_owned());
        let matches = name.as_ref().and_then(|name| index.files().get(name));
        match matches.map(Vec::as_slice) {
            Some([only]) => {
                if let Some(new) = written_as(&found, written, url, dir, only) {
                    edits.push(LinkEdit {
                        range: found.range.clone(),
                        old: written.to_owned(),
                        new,
                    });
                }
            }
            _ => unresolved.push(written.to_owned()),
        }
    }
    (edits, unresolved)
}

fn read_document(path: &Path) -> Result<String, &'static str> {
    let meta = disk::metadata(path).map_err(|_| "couldn't be read")?;
    if meta.len() > MAX_DOCUMENT {
        return Err("is too large");
    }
    let bytes = disk::read(path).map_err(|_| "couldn't be read")?;
    String::from_utf8(bytes).map_err(|_| "isn't UTF-8")
}

fn changes_in(path: &Path, moves: &[Moved]) -> Result<Option<DocumentChange>, &'static str> {
    let source = read_document(path)?;
    let dir = path.parent().ok_or("couldn't be read")?;
    let vault = links::marked_vault(dir).unwrap_or_else(|| dir.to_path_buf());
    let index = Index::new(&vault);
    let nearest = |name: &str| index.nearest(name);
    let world = World {
        exists: &exists,
        nearest: &nearest,
    };
    let edits = edits_for(&source, path, &vault, moves, &world);
    Ok((!edits.is_empty()).then(|| DocumentChange {
        path: path.to_path_buf(),
        vault,
        edits,
    }))
}

/// Makes the changes, working them out anew from what's in the file now: it
/// may have changed since they were shown. What couldn't be done, and why.
pub fn apply(proposal: &Proposal, skip: &[PathBuf]) -> Vec<String> {
    let mut problems = Vec::new();
    for document in &proposal.documents {
        if skip.contains(&document.path) {
            continue;
        }
        let name = crate::fs::display_name(&document.path);
        let source = match read_document(&document.path) {
            Ok(source) => source,
            Err(why) => {
                problems.push(format!("{name} {why}"));
                continue;
            }
        };
        let index = Index::new(&document.vault);
        let edits = if proposal.repair.is_some() {
            repairs_for(&source, &document.path, &index).0
        } else {
            let nearest = |name: &str| index.nearest(name);
            let world = World {
                exists: &exists,
                nearest: &nearest,
            };
            edits_for(
                &source,
                &document.path,
                &document.vault,
                &proposal.moves,
                &world,
            )
        };
        if edits.is_empty() {
            continue;
        }
        let text = apply_edits(&source, &edits);
        if let Err(err) = crate::editor::document::write_atomic(&document.path, text.as_bytes()) {
            problems.push(format!("{name}: {err}"));
        }
    }
    problems
}

/// Keeps what's being looked for, and what's waiting to be asked about.
pub struct Relinker {
    sender: Sender<Proposal>,
    results: Receiver<Proposal>,
    problems_sender: Sender<Vec<String>>,
    problems_results: Receiver<Vec<String>>,
    waiting: std::collections::VecDeque<Proposal>,
    problems: Vec<String>,
}

impl Default for Relinker {
    fn default() -> Self {
        let (sender, results) = mpsc::channel();
        let (problems_sender, problems_results) = mpsc::channel();
        Self {
            sender,
            results,
            problems_sender,
            problems_results,
            waiting: Default::default(),
            problems: Vec::new(),
        }
    }
}

impl Relinker {
    /// Things moved: looks for links to change, in the background.
    pub fn moved(&self, ctx: &egui::Context, moves: Vec<Moved>) {
        if moves.is_empty() {
            return;
        }
        let sender = self.sender.clone();
        let ctx = ctx.clone();
        crate::background::spawn("markdown-relink-scan", move || {
            let _ = sender.send(scan(moves));
            ctx.request_repaint();
        });
    }

    /// Looks for broken links around this document, in the background.
    pub fn repair(&self, ctx: &egui::Context, document: PathBuf) {
        let sender = self.sender.clone();
        let ctx = ctx.clone();
        crate::background::spawn("markdown-relink-repair", move || {
            let _ = sender.send(repair(document));
            ctx.request_repaint();
        });
    }

    /// Asks about what was found. `unsaved` says which documents have changes
    /// in an editor, which aren't touched: they'd be lost or lose these.
    pub fn ui(&mut self, ctx: &egui::Context, unsaved: &dyn Fn(&Path) -> bool) {
        for proposal in self.results.try_iter() {
            // Asked for, it says what it found even if that's nothing.
            if !proposal.documents.is_empty() || proposal.repair.is_some() {
                self.waiting.push_back(proposal);
            }
        }
        self.problems
            .extend(self.problems_results.try_iter().flatten());
        if !self.problems.is_empty() {
            self.problems_ui(ctx);
            return;
        }
        let Some(proposal) = self.waiting.front() else {
            return;
        };
        let skipped: Vec<PathBuf> = proposal
            .documents
            .iter()
            .filter(|document| unsaved(&document.path))
            .map(|document| document.path.clone())
            .collect();
        let mut answer = None;
        let modal = egui::Modal::new(egui::Id::new("relink")).show(ctx, |ui| {
            answer = proposal_ui(ui, proposal, &skipped);
        });
        if modal.should_close() && answer.is_none() {
            answer = Some(false);
        }
        let Some(update) = answer else {
            return;
        };
        let Some(proposal) = self.waiting.pop_front() else {
            return;
        };
        if update {
            let sender = self.problems_sender.clone();
            let ctx = ctx.clone();
            crate::background::spawn("markdown-relink-apply", move || {
                let _ = sender.send(apply(&proposal, &skipped));
                ctx.request_repaint();
            });
        }
    }

    fn problems_ui(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("relink-problems")).show(ctx, |ui| {
            ui.set_max_width(crate::widgets::modal_width(ui, 520.0));
            ui.heading("Some links couldn't be updated");
            for problem in &self.problems {
                ui.label(problem);
            }
            ui.add_space(8.0);
            close = ui.button("OK").clicked();
        });
        if close || modal.should_close() {
            self.problems.clear();
        }
    }
}

/// The list of changes, and whether to make them when that's been answered.
fn proposal_ui(ui: &mut egui::Ui, proposal: &Proposal, skipped: &[PathBuf]) -> Option<bool> {
    // What it would do, which isn't what it has to leave.
    let (mut links, mut documents) = (0, 0);
    for document in &proposal.documents {
        if !skipped.contains(&document.path) {
            links += document.edits.len();
            documents += 1;
        }
    }
    ui.set_max_width(crate::widgets::modal_width(ui, 640.0));
    let plural = |count: usize, word: &str| {
        if count == 1 {
            format!("{count} {word}")
        } else {
            format!("{count} {word}s")
        }
    };
    if documents > 0 {
        ui.heading(format!(
            "{} {} in {}?",
            if proposal.repair.is_some() {
                "Repair"
            } else {
                "Update"
            },
            plural(links, "link"),
            plural(documents, "document")
        ));
    } else if !proposal.documents.is_empty() {
        ui.heading("Links can't be updated");
    }
    if proposal.repair.is_some() && proposal.documents.is_empty() {
        ui.heading("No links to repair");
    }
    if proposal.repair.is_none() {
        ui.label("What moved leaves these pointing the wrong way.");
    }
    ui.add_space(4.0);
    egui::ScrollArea::vertical()
        .max_height(ui.ctx().content_rect().height() * 0.5)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for document in &proposal.documents {
                let name = document
                    .path
                    .strip_prefix(&document.vault)
                    .unwrap_or(&document.path)
                    .display()
                    .to_string();
                ui.add_space(4.0);
                ui.label(egui::RichText::new(name).strong());
                if skipped.contains(&document.path) {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        "Has unsaved changes, so it won't be updated.",
                    );
                }
                for edit in &document.edits {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("{} → {}", edit.old, edit.new)).monospace(),
                        )
                        .wrap(),
                    );
                }
            }
        });
    if !proposal.unresolved.is_empty() {
        ui.add_space(4.0);
        ui.collapsing(
            format!(
                "{} that can't be worked out, with no file or several of that name",
                plural(proposal.unresolved.len(), "broken link")
            ),
            |ui| {
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .show(ui, |ui| {
                        for (document, link) in &proposal.unresolved {
                            let name = crate::fs::display_name(document);
                            ui.label(egui::RichText::new(format!("{name}: {link}")).monospace());
                        }
                    });
            },
        );
    }
    if !proposal.unread.is_empty() {
        ui.add_space(4.0);
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!(
                "{} not looked through:",
                plural(proposal.unread.len(), "document")
            ),
        );
        for (document, why) in &proposal.unread {
            ui.label(format!("{} {why}", crate::fs::display_name(document)));
        }
    }
    if proposal.incomplete {
        ui.add_space(4.0);
        ui.colored_label(
            ui.visuals().warn_fg_color,
            "There were too many files to look through them all, so there may be more.",
        );
    }
    ui.add_space(8.0);
    let mut answer = None;
    ui.horizontal(|ui| {
        let update = if proposal.repair.is_some() {
            "Repair links"
        } else {
            "Update links"
        };
        if documents > 0 && ui.button(update).clicked() {
            answer = Some(true);
        }
        let leave = if documents > 0 { "Leave them" } else { "Close" };
        if ui.button(leave).clicked() {
            answer = Some(false);
        }
    });
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moved(from: &str, to: &str) -> Moved {
        Moved {
            from: from.into(),
            to: to.into(),
        }
    }

    /// What's on the disk after the moves, for these tests.
    fn after(source: &str, document: &str, moves: &[Moved], files: &[&str]) -> String {
        let exists = |path: &Path| files.iter().any(|known| Path::new(known) == path);
        let nearest = |name: &str| {
            files
                .iter()
                .filter(|known| Path::new(known).file_name().and_then(|n| n.to_str()) == Some(name))
                .min_by_key(|known| depth(Path::new(known)))
                .map(PathBuf::from)
        };
        let world = World {
            exists: &exists,
            nearest: &nearest,
        };
        let edits = edits_for(source, Path::new(document), Path::new("/v"), moves, &world);
        apply_edits(source, &edits)
    }

    #[test]
    fn a_link_follows_what_moved() {
        let moves = [moved("/v/a.png", "/v/pics/a.png")];
        assert_eq!(
            after("![x](a.png)\n", "/v/d.md", &moves, &["/v/pics/a.png"]),
            "![x](pics/a.png)\n"
        );
        assert_eq!(
            after("[x](../a.png)\n", "/v/sub/d.md", &moves, &["/v/pics/a.png"]),
            "[x](../pics/a.png)\n"
        );
    }

    #[test]
    fn a_moved_document_fixes_its_own_links() {
        let moves = [moved("/v/d.md", "/v/deep/er/d.md")];
        assert_eq!(
            after(
                "![x](pics/a.png)\n",
                "/v/deep/er/d.md",
                &moves,
                &["/v/pics/a.png"]
            ),
            "![x](../../pics/a.png)\n"
        );
    }

    #[test]
    fn what_moved_together_stays_as_it_is() {
        let moves = [moved("/v/old", "/v/new")];
        let source = "![x](pics/a.png) [y](d2.md)\n";
        assert_eq!(
            after(
                source,
                "/v/new/d.md",
                &moves,
                &["/v/new/pics/a.png", "/v/new/d2.md"]
            ),
            source
        );
    }

    #[test]
    fn only_the_destination_changes() {
        let source = "Odd  spacing\r\n\r\n* [te*x*t](a.png  \"ti(t)le\")  and [b](b.png)\t\r\n";
        let moves = [moved("/v/a.png", "/v/p/a.png")];
        assert_eq!(
            after(source, "/v/d.md", &moves, &["/v/p/a.png", "/v/b.png"]),
            "Odd  spacing\r\n\r\n* [te*x*t](p/a.png  \"ti(t)le\")  and [b](b.png)\t\r\n"
        );
    }

    #[test]
    fn a_link_that_was_already_broken_is_left_alone() {
        let moves = [moved("/v/d.md", "/v/sub/d.md")];
        let source = "[gone](missing.md)\n";
        assert_eq!(after(source, "/v/sub/d.md", &moves, &[]), source);
    }

    #[test]
    fn what_is_not_a_local_link_is_left_alone() {
        let moves = [moved("/v/d.md", "/v/sub/d.md")];
        let source = "[a](https://x.org/a.png) [b](#here) [c](mailto:a@b.c) `[d](d.md)`\n\n```\n[e](e.md)\n```\n";
        let everything = &["/v/d.md", "/v/e.md", "/v/sub/d.md"];
        assert_eq!(after(source, "/v/sub/d.md", &moves, everything), source);
    }

    #[test]
    fn headings_and_queries_stay_on_the_link() {
        let moves = [moved("/v/a.md", "/v/s/a.md")];
        assert_eq!(
            after(
                "[x](a.md#some-heading)\n",
                "/v/d.md",
                &moves,
                &["/v/s/a.md"]
            ),
            "[x](s/a.md#some-heading)\n"
        );
    }

    #[test]
    fn spaces_are_written_as_the_link_was() {
        let moves = [moved("/v/a b.png", "/v/my pics/a b.png")];
        let files = &["/v/my pics/a b.png"];
        assert_eq!(
            after("![x](a%20b.png)\n", "/v/d.md", &moves, files),
            "![x](my%20pics/a%20b.png)\n"
        );
        assert_eq!(
            after("![x](<a b.png>)\n", "/v/d.md", &moves, files),
            "![x](<my pics/a b.png>)\n"
        );
    }

    #[test]
    fn a_dot_slash_stays() {
        let moves = [moved("/v/a.png", "/v/p/a.png")];
        assert_eq!(
            after("![x](./a.png)\n", "/v/d.md", &moves, &["/v/p/a.png"]),
            "![x](./p/a.png)\n"
        );
    }

    #[test]
    fn absolute_links_stay_absolute() {
        let moves = [moved("/v/a.png", "/v/p/a.png")];
        let files = &["/v/p/a.png"];
        assert_eq!(
            after("![x](/v/a.png)\n", "/v/d.md", &moves, files),
            "![x](/v/p/a.png)\n"
        );
        assert_eq!(
            after("![x](file:///v/a.png)\n", "/v/d.md", &moves, files),
            "![x](file:///v/p/a.png)\n"
        );
    }

    #[test]
    fn reference_definitions_are_links_too() {
        let moves = [moved("/v/a.png", "/v/p/a.png")];
        assert_eq!(
            after(
                "![x][ref] and [ref]\n\n[ref]: a.png \"Title\"\n",
                "/v/d.md",
                &moves,
                &["/v/p/a.png"]
            ),
            "![x][ref] and [ref]\n\n[ref]: p/a.png \"Title\"\n"
        );
    }

    #[test]
    fn a_folder_moved_takes_what_is_in_it() {
        let moves = [moved("/v/pics", "/v/assets/pics")];
        assert_eq!(
            after(
                "![x](pics/deep/a.png)\n",
                "/v/d.md",
                &moves,
                &["/v/assets/pics/deep/a.png"]
            ),
            "![x](assets/pics/deep/a.png)\n"
        );
    }

    #[test]
    fn a_name_is_not_matched_by_a_folder_that_starts_the_same() {
        let moves = [moved("/v/pics", "/v/assets")];
        let source = "![x](pics2/a.png)\n";
        assert_eq!(
            after(source, "/v/d.md", &moves, &["/v/pics2/a.png"]),
            source
        );
    }

    #[test]
    fn a_wikilink_by_name_only_changes_with_the_notes_name() {
        let source = "[[Note]] and [[Note|shown]] and [[Note#Head]]\n";
        // It finds its way wherever the note moved.
        let moves = [moved("/v/Note.md", "/v/sub/Note.md")];
        assert_eq!(
            after(source, "/v/d.md", &moves, &["/v/sub/Note.md"]),
            source
        );
        let moves = [moved("/v/Note.md", "/v/New name.md")];
        assert_eq!(
            after(source, "/v/d.md", &moves, &["/v/New name.md"]),
            "[[New name]] and [[New name|shown]] and [[New name#Head]]\n"
        );
    }

    #[test]
    fn a_wikilink_with_a_path_follows_it() {
        let source = "[[notes/Note|shown]]\n";
        let moves = [moved("/v/notes/Note.md", "/v/kept/Note.md")];
        assert_eq!(
            after(source, "/v/d.md", &moves, &["/v/kept/Note.md"]),
            "[[kept/Note|shown]]\n"
        );
    }

    #[test]
    fn relative_ways_are_found() {
        let way = |from: &str, to: &str| relative(Path::new(from), Path::new(to));
        assert_eq!(way("/a/b", "/a/c.md").as_deref(), Some("../c.md"));
        assert_eq!(way("/a", "/a/b/c.md").as_deref(), Some("b/c.md"));
        assert_eq!(way("/a/b", "/x/y.md").as_deref(), Some("../../x/y.md"));
    }

    fn fixture(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("egui-files-relink-{name}-{}", std::process::id()));
        let _ = disk::remove_dir_all(&root);
        disk::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_scan_finds_and_changes_documents_in_the_repository() {
        let root = fixture("repo");
        disk::create_dir_all(root.join(".git")).unwrap();
        disk::create_dir_all(root.join("docs/deep")).unwrap();
        disk::create_dir_all(root.join("pics")).unwrap();
        disk::create_dir_all(root.join(".hidden")).unwrap();
        disk::write(root.join("a.png"), "").unwrap();
        let top = "![x](a.png)\r\n";
        let deep = "Some text.\n\n![x](../../a.png)\n";
        disk::write(root.join("top.md"), top).unwrap();
        disk::write(root.join("docs/deep/deep.md"), deep).unwrap();
        disk::write(root.join("other.md"), "no links\n").unwrap();
        disk::write(root.join(".hidden/h.md"), "![x](../a.png)\n").unwrap();
        // It moved, and then it's looked for.
        let (from, to) = (root.join("a.png"), root.join("pics/a.png"));
        disk::rename(&from, &to).unwrap();
        let proposal = scan(vec![Moved { from, to }]);
        let names: Vec<String> = proposal
            .documents
            .iter()
            .map(|d| d.path.strip_prefix(&root).unwrap().display().to_string())
            .collect();
        assert_eq!(names.len(), 3, "hidden folders too: {names:?}");
        assert!(names.iter().any(|name| name == "top.md"));
        assert!(!proposal.incomplete);

        assert_eq!(apply(&proposal, &[]), Vec::<String>::new());
        assert_eq!(
            disk::read_to_string(root.join("top.md")).unwrap(),
            "![x](pics/a.png)\r\n"
        );
        assert_eq!(
            disk::read_to_string(root.join("docs/deep/deep.md")).unwrap(),
            "Some text.\n\n![x](../../pics/a.png)\n"
        );
        let _ = disk::remove_dir_all(&root);
    }

    #[test]
    fn without_a_repository_only_the_folders_involved_are_looked_through() {
        let root = fixture("plain");
        disk::create_dir_all(root.join("sub")).unwrap();
        disk::create_dir_all(root.join("elsewhere")).unwrap();
        disk::create_dir_all(root.join("to")).unwrap();
        disk::write(root.join("beside.md"), "![x](a.png)\n").unwrap();
        disk::write(root.join("sub/below.md"), "![x](../a.png)\n").unwrap();
        disk::write(root.join("a.png"), "").unwrap();
        let (from, to) = (root.join("a.png"), root.join("to/a.png"));
        disk::rename(&from, &to).unwrap();
        let proposal = scan(vec![Moved { from, to }]);
        let docs: Vec<&Path> = proposal
            .documents
            .iter()
            .map(|d| d.path.as_path())
            .collect();
        assert_eq!(docs, [root.join("beside.md")]);
        let _ = disk::remove_dir_all(&root);
    }

    #[test]
    fn a_moved_document_on_disk_fixes_its_own_links() {
        let root = fixture("own");
        disk::create_dir_all(root.join("deep")).unwrap();
        disk::write(root.join("a.png"), "").unwrap();
        disk::write(root.join("d.md"), "![x](a.png)\n").unwrap();
        let (from, to) = (root.join("d.md"), root.join("deep/d.md"));
        disk::rename(&from, &to).unwrap();
        let proposal = scan(vec![Moved {
            from,
            to: to.clone(),
        }]);
        assert_eq!(apply(&proposal, &[]), Vec::<String>::new());
        assert_eq!(disk::read_to_string(&to).unwrap(), "![x](../a.png)\n");
        let _ = disk::remove_dir_all(&root);
    }

    #[test]
    fn what_has_unsaved_changes_is_not_touched() {
        let root = fixture("unsaved");
        disk::create_dir_all(root.join("to")).unwrap();
        disk::write(root.join("a.png"), "").unwrap();
        disk::write(root.join("d.md"), "![x](a.png)\n").unwrap();
        let (from, to) = (root.join("a.png"), root.join("to/a.png"));
        disk::rename(&from, &to).unwrap();
        let proposal = scan(vec![Moved { from, to }]);
        assert_eq!(proposal.documents.len(), 1);
        apply(&proposal, &[root.join("d.md")]);
        assert_eq!(
            disk::read_to_string(root.join("d.md")).unwrap(),
            "![x](a.png)\n"
        );
        let _ = disk::remove_dir_all(&root);
    }

    #[test]
    fn changes_are_worked_out_anew_when_made() {
        let root = fixture("changed");
        disk::create_dir_all(root.join("to")).unwrap();
        disk::write(root.join("a.png"), "").unwrap();
        disk::write(root.join("d.md"), "![x](a.png)\n").unwrap();
        let (from, to) = (root.join("a.png"), root.join("to/a.png"));
        disk::rename(&from, &to).unwrap();
        let proposal = scan(vec![Moved { from, to }]);
        // Edited in the meantime: the offsets found are no good anymore.
        disk::write(root.join("d.md"), "Added first.\n\n![x](a.png)\n").unwrap();
        apply(&proposal, &[]);
        assert_eq!(
            disk::read_to_string(root.join("d.md")).unwrap(),
            "Added first.\n\n![x](to/a.png)\n"
        );
        let _ = disk::remove_dir_all(&root);
    }

    /// Every link in the showcase, there and back: what comes back is what went.
    #[test]
    fn a_document_moved_there_and_back_is_as_it_was() {
        let source = include_str!("../../samples/markdown/showcase.md");
        let (home, away) = ("/v/docs/showcase.md", "/v/else/where/showcase.md");
        let world = World {
            // Notes aren't beside it, either side, or the name would need a path.
            exists: &|path: &Path| {
                !["other.md", "a note.md", "No such note.md"]
                    .iter()
                    .any(|note| path.ends_with(note))
            },
            nearest: &|_: &str| None,
        };
        let there = [moved(home, away)];
        let edits = edits_for(source, Path::new(away), Path::new("/v"), &there, &world);
        assert!(edits.len() > 5, "{}", edits.len());
        for edit in &edits {
            assert_eq!(
                &source[edit.range.clone()],
                edit.old,
                "the bytes it says it changes"
            );
        }
        let moved_away = apply_edits(source, &edits);
        let back = [moved(away, home)];
        let edits = edits_for(&moved_away, Path::new(home), Path::new("/v"), &back, &world);
        assert_eq!(apply_edits(&moved_away, &edits), source);
    }

    #[test]
    fn raw_html_follows_too() {
        let moves = [moved("/v/a.png", "/v/p i/a.png")];
        let files = &["/v/p i/a.png"];
        assert_eq!(
            after(
                "<img class=\"x\" SRC=\"a.png\" alt='a.png'>\n",
                "/v/d.md",
                &moves,
                files
            ),
            "<img class=\"x\" SRC=\"p%20i/a.png\" alt='a.png'>\n"
        );
        assert_eq!(
            after(
                "Text <a href='a.png'>x</a> more\n",
                "/v/d.md",
                &moves,
                files
            ),
            "Text <a href='p%20i/a.png'>x</a> more\n"
        );
        for untouched in [
            "<!-- <img src=\"a.png\"> -->\n",
            "<!-- one\n<img src=\"a.png\">\nthree -->\n",
            "`<img src=\"a.png\">`\n",
            "<img data-src=\"a.png\">\n",
            "<a href=\"https://x.org/a.png\">x</a>\n",
        ] {
            assert_eq!(after(untouched, "/v/d.md", &moves, files), untouched);
        }
    }

    #[test]
    fn a_renamed_note_keeps_links_that_meant_another_of_its_name() {
        // Beside the document is what it means, and that one stayed.
        let moves = [moved("/v/far/Note.md", "/v/far/Renamed.md")];
        let source = "[[Note]]\n";
        assert_eq!(
            after(
                source,
                "/v/d/doc.md",
                &moves,
                &["/v/d/Note.md", "/v/far/Renamed.md"]
            ),
            source
        );
        // Nearer the top is what it means, and that one stayed.
        let moves = [moved("/v/a/b/c/Note.md", "/v/a/b/c/Renamed.md")];
        assert_eq!(
            after(
                source,
                "/v/doc.md",
                &moves,
                &["/v/x/Note.md", "/v/a/b/c/Renamed.md"]
            ),
            source
        );
        // The one renamed is what it meant, being nearest.
        let moves = [moved("/v/x/Note.md", "/v/x/Renamed.md")];
        assert_eq!(
            after(
                source,
                "/v/doc.md",
                &moves,
                &["/v/a/b/c/Note.md", "/v/x/Renamed.md"]
            ),
            "[[Renamed]]\n"
        );
    }

    #[test]
    fn a_name_that_would_find_another_note_gets_a_path() {
        // It meant the one beside it, which moved away from another of that name.
        let moves = [moved("/v/d/Note.md", "/v/deep/er/Note.md")];
        assert_eq!(
            after(
                "[[Note|shown]]\n",
                "/v/d/doc.md",
                &moves,
                &["/v/x/Note.md", "/v/deep/er/Note.md"]
            ),
            "[[deep/er/Note|shown]]\n"
        );
    }

    #[test]
    fn repairs_find_what_moved_by_its_name() {
        let root = fixture("repair");
        disk::create_dir_all(root.join(".git")).unwrap();
        disk::create_dir_all(root.join("docs")).unwrap();
        disk::create_dir_all(root.join("elsewhere/a")).unwrap();
        disk::create_dir_all(root.join("elsewhere/b")).unwrap();
        // Moved by something else: this link is broken.
        disk::write(root.join("elsewhere/logo.png"), "").unwrap();
        disk::write(root.join("elsewhere/a/twin.png"), "").unwrap();
        disk::write(root.join("elsewhere/b/twin.png"), "").unwrap();
        disk::write(root.join("fine.png"), "").unwrap();
        let doc = root.join("docs/doc.md");
        let source = "![x](logo.png) ![y](twin.png) ![z](none.png) ![w](../fine.png) [web](https://a.b/c.png)\n";
        disk::write(&doc, source).unwrap();
        let proposal = repair(doc.clone());
        assert_eq!(proposal.documents.len(), 1);
        let edits = &proposal.documents[0].edits;
        assert_eq!(edits.len(), 1, "{edits:?}");
        assert_eq!(edits[0].new, "../elsewhere/logo.png");
        let unresolved: Vec<&str> = proposal
            .unresolved
            .iter()
            .map(|(_, u)| u.as_str())
            .collect();
        assert_eq!(
            unresolved,
            ["twin.png", "none.png"],
            "several, and none, of that name"
        );

        assert_eq!(apply(&proposal, &[]), Vec::<String>::new());
        assert_eq!(
            disk::read_to_string(&doc).unwrap(),
            source.replace("![x](logo.png)", "![x](../elsewhere/logo.png)")
        );
        let again = repair(doc);
        assert!(again.documents.is_empty(), "nothing more to do");
        let _ = disk::remove_dir_all(&root);
    }

    #[test]
    fn what_cant_be_read_is_said() {
        let root = fixture("unread");
        disk::create_dir_all(root.join("to")).unwrap();
        disk::write(root.join("a.png"), "").unwrap();
        disk::write(root.join("bad.md"), [0xFF, 0xFE, 0x00]).unwrap();
        disk::write(root.join("d.md"), "![x](a.png)\n").unwrap();
        let (from, to) = (root.join("a.png"), root.join("to/a.png"));
        disk::rename(&from, &to).unwrap();
        let proposal = scan(vec![Moved { from, to }]);
        assert_eq!(proposal.unread, [(root.join("bad.md"), "isn't UTF-8")]);
        assert_eq!(proposal.documents.len(), 1);
        let _ = disk::remove_dir_all(&root);
    }

    #[test]
    fn tools_folders_are_not_looked_through() {
        let root = fixture("tools");
        disk::create_dir_all(root.join(".git")).unwrap();
        disk::create_dir_all(root.join("node_modules/x")).unwrap();
        disk::create_dir_all(root.join(".github")).unwrap();
        disk::create_dir_all(root.join("to")).unwrap();
        disk::write(root.join("a.png"), "").unwrap();
        disk::write(root.join("node_modules/x/r.md"), "![x](../../a.png)\n").unwrap();
        disk::write(root.join(".git/r.md"), "![x](../a.png)\n").unwrap();
        disk::write(root.join(".github/r.md"), "![x](../a.png)\n").unwrap();
        let (from, to) = (root.join("a.png"), root.join("to/a.png"));
        disk::rename(&from, &to).unwrap();
        let proposal = scan(vec![Moved { from, to }]);
        let docs: Vec<&Path> = proposal
            .documents
            .iter()
            .map(|d| d.path.as_path())
            .collect();
        assert_eq!(
            docs,
            [root.join(".github/r.md")],
            "what's written to be read"
        );
        let _ = disk::remove_dir_all(&root);
    }
}
