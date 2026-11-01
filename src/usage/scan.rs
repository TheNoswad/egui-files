//! Walks a folder tree on worker threads, adding each folder's sizes to its
//! ancestors as soon as it's read, so the charts can show a scan that's still
//! running.

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use egui::Context;
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

/// No node: the root's parent, the end of a list of children.
pub const NONE: u32 = u32::MAX;
pub const ROOT: u32 = 0;

/// What files are, roughly, for the file type breakdown and colors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    Video,
    Image,
    Audio,
    Document,
    Code,
    Archive,
    Program,
    DiskImage,
    Other,
}

pub const CATEGORIES: usize = 9;

impl Category {
    pub const ALL: [Self; CATEGORIES] = [
        Self::Video,
        Self::Image,
        Self::Audio,
        Self::Document,
        Self::Code,
        Self::Archive,
        Self::Program,
        Self::DiskImage,
        Self::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Video => "Videos",
            Self::Image => "Images",
            Self::Audio => "Audio",
            Self::Document => "Documents",
            Self::Code => "Code",
            Self::Archive => "Archives",
            Self::Program => "Programs & libraries",
            Self::DiskImage => "Disk images",
            Self::Other => "Other",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Video => icon::FILE_VIDEO,
            Self::Image => icon::FILE_IMAGE,
            Self::Audio => icon::FILE_AUDIO,
            Self::Document => icon::FILE_TEXT,
            Self::Code => icon::FILE_CODE,
            Self::Archive => icon::FILE_ZIP,
            Self::Program => icon::GEAR,
            Self::DiskImage => icon::DISC,
            Self::Other => icon::FILE,
        }
    }

    /// From the file name's extension; files without one are "other".
    pub fn of(name: &OsStr) -> Self {
        let Some(ext) = Path::new(name).extension().and_then(OsStr::to_str) else {
            return Self::Other;
        };
        match ext.to_ascii_lowercase().as_str() {
            "mp4" | "mkv" | "webm" | "avi" | "mov" | "wmv" | "m4v" | "mpg" | "mpeg" | "ts"
            | "m2ts" | "flv" | "3gp" | "ogv" => Self::Video,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico" | "tif" | "tiff"
            | "heic" | "heif" | "avif" | "raw" | "cr2" | "nef" | "arw" | "dng" | "psd" | "xcf"
            | "kra" | "exr" => Self::Image,
            "mp3" | "flac" | "ogg" | "wav" | "opus" | "m4a" | "aac" | "wma" | "aiff" | "mid" => {
                Self::Audio
            }
            "pdf" | "txt" | "md" | "doc" | "docx" | "odt" | "rtf" | "xls" | "xlsx" | "ods"
            | "ppt" | "pptx" | "odp" | "epub" | "csv" | "log" | "tex" => Self::Document,
            "rs" | "py" | "js" | "tsx" | "jsx" | "c" | "h" | "cpp" | "hpp" | "cc" | "go"
            | "java" | "kt" | "cs" | "sh" | "fish" | "toml" | "json" | "yaml" | "yml" | "html"
            | "css" | "lua" | "ron" | "rb" | "php" | "swift" | "zig" | "xml" | "o" | "rlib"
            | "rmeta" | "d" | "pyc" | "class" | "jar" | "wasm" => Self::Code,
            "zip" | "tar" | "gz" | "tgz" | "xz" | "zst" | "7z" | "rar" | "bz2" | "lz4" | "lzma"
            | "cab" | "deb" | "rpm" | "pkg" | "apk" | "appimage" | "flatpak" | "whl" | "crate" => {
                Self::Archive
            }
            "exe" | "dll" | "so" | "msi" | "sys" | "bin" | "elf" | "dylib" | "a" | "lib"
            | "node" | "pak" | "dat" | "vpk" => Self::Program,
            "iso" | "img" | "qcow2" | "vdi" | "vmdk" | "vhd" | "vhdx" | "raw.xz" | "wim"
            | "dmg" | "swap" => Self::DiskImage,
            _ => Self::Other,
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

/// Which size to add up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Metric {
    /// Space taken on the disk: whole blocks, sparse files' holes left out.
    #[default]
    Disk,
    /// The files' lengths.
    Apparent,
}

impl Metric {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disk => "Size on disk",
            Self::Apparent => "Apparent size",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File(Category),
    Symlink,
    /// Another filesystem mounted here; not scanned.
    Mount,
}

/// Added up over a folder's contents.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Totals {
    pub disk: u64,
    pub apparent: u64,
    pub files: u64,
    /// `disk` and `apparent` per category.
    pub by_category: [[u64; CATEGORIES]; 2],
}

impl Totals {
    fn add(&mut self, other: &Totals) {
        self.disk = self.disk.wrapping_add(other.disk);
        self.apparent = self.apparent.wrapping_add(other.apparent);
        self.files = self.files.wrapping_add(other.files);
        for (mine, theirs) in self.by_category.iter_mut().zip(&other.by_category) {
            for (a, b) in mine.iter_mut().zip(theirs) {
                *a = a.wrapping_add(*b);
            }
        }
    }

    /// Adding the result takes `self` away again (sizes are unsigned).
    fn negated(&self) -> Totals {
        let mut neg = Totals {
            disk: self.disk.wrapping_neg(),
            apparent: self.apparent.wrapping_neg(),
            files: self.files.wrapping_neg(),
            by_category: self.by_category,
        };
        for row in &mut neg.by_category {
            for v in row {
                *v = v.wrapping_neg();
            }
        }
        neg
    }

    pub fn size(&self, metric: Metric) -> u64 {
        match metric {
            Metric::Disk => self.disk,
            Metric::Apparent => self.apparent,
        }
    }

    pub fn category(&self, metric: Metric, category: Category) -> u64 {
        self.by_category[metric as usize][category.index()]
    }
}

pub struct Node {
    pub name: Box<OsStr>,
    /// `NONE` for the root, and for nodes taken out of the tree.
    pub parent: u32,
    pub first_child: u32,
    pub next_sibling: u32,
    pub kind: Kind,
    pub totals: Totals,
    /// The newest modification time inside, in seconds since 1970.
    pub newest: i64,
    /// A folder that couldn't be listed.
    pub unreadable: bool,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    pub fn size(&self, metric: Metric) -> u64 {
        self.totals.size(metric)
    }

    /// The category a file is, or the one taking most of a folder.
    pub fn category(&self, metric: Metric) -> Category {
        match self.kind {
            Kind::File(category) => category,
            Kind::Dir => {
                let sizes = &self.totals.by_category[metric as usize];
                Category::ALL
                    .into_iter()
                    .max_by_key(|c| sizes[c.index()])
                    .unwrap_or(Category::Other)
            }
            Kind::Symlink | Kind::Mount => Category::Other,
        }
    }
}

pub struct Tree {
    nodes: Vec<Node>,
    pub root_path: PathBuf,
    /// Changes whenever the tree does, for caches built from it.
    pub generation: u64,
    pub dirs: u64,
    pub unreadable: u64,
    /// Files with more than one link already counted, by device and inode.
    links: HashSet<(u64, u64)>,
}

impl Tree {
    fn new(root_path: PathBuf) -> Self {
        Self {
            nodes: vec![Node {
                name: root_path.as_os_str().into(),
                parent: NONE,
                first_child: NONE,
                next_sibling: NONE,
                kind: Kind::Dir,
                totals: Totals::default(),
                newest: 0,
                unreadable: false,
            }],
            root_path,
            generation: 0,
            dirs: 1,
            unreadable: 0,
            links: HashSet::new(),
        }
    }

    pub fn node(&self, id: u32) -> &Node {
        &self.nodes[id as usize]
    }

    pub fn children(&self, id: u32) -> impl Iterator<Item = u32> + '_ {
        let mut next = self.node(id).first_child;
        std::iter::from_fn(move || {
            (next != NONE).then(|| {
                let id = next;
                next = self.node(id).next_sibling;
                id
            })
        })
    }

    /// Biggest first, leaving out empty ones.
    pub fn sorted_children(&self, id: u32, metric: Metric) -> Vec<u32> {
        let mut children: Vec<u32> = self
            .children(id)
            .filter(|&c| self.node(c).size(metric) > 0)
            .collect();
        children.sort_by_key(|&c| std::cmp::Reverse(self.node(c).size(metric)));
        children
    }

    /// Whether `id` is `ancestor` or inside it.
    pub fn is_within(&self, mut id: u32, ancestor: u32) -> bool {
        while id != NONE {
            if id == ancestor {
                return true;
            }
            id = self.node(id).parent;
        }
        false
    }

    /// From the root down to `id`.
    pub fn ancestry(&self, mut id: u32) -> Vec<u32> {
        let mut chain = Vec::new();
        while id != NONE {
            chain.push(id);
            id = self.node(id).parent;
        }
        chain.reverse();
        chain
    }

    /// Whether `id` is still in the tree (not removed or rescanned away).
    pub fn is_attached(&self, id: u32) -> bool {
        (id as usize) < self.nodes.len() && self.ancestry(id).first() == Some(&ROOT)
    }

    pub fn path(&self, id: u32) -> PathBuf {
        let mut path = self.root_path.clone();
        for &id in &self.ancestry(id)[1..] {
            path.push(&*self.node(id).name);
        }
        path
    }

    pub fn name(&self, id: u32) -> String {
        if id == ROOT {
            crate::fs::display_name(&self.root_path)
        } else {
            self.node(id).name.to_string_lossy().into_owned()
        }
    }

    pub fn find(&self, path: &Path) -> Option<u32> {
        let rest = path.strip_prefix(&self.root_path).ok()?;
        let mut id = ROOT;
        for part in rest.components() {
            let part = part.as_os_str();
            id = self.children(id).find(|&c| &*self.node(c).name == part)?;
        }
        Some(id)
    }

    /// Adds to a node and everything above it.
    fn add_up(&mut self, mut id: u32, totals: &Totals, newest: i64) {
        while id != NONE {
            let node = &mut self.nodes[id as usize];
            node.totals.add(totals);
            node.newest = node.newest.max(newest);
            id = node.parent;
        }
    }

    /// Adds a folder's listing; returns the subfolders to scan next.
    fn add_children(&mut self, parent: u32, entries: Vec<Scanned>) -> Vec<(u32, PathBuf)> {
        let mut dirs = Vec::new();
        let mut sum = Totals::default();
        let mut newest = 0;
        let parent_path = self.path(parent);
        for entry in entries {
            let mut totals = Totals::default();
            if let Kind::File(category) = entry.kind {
                // Hard links take the space once.
                let counted = entry.link.is_some_and(|link| !self.links.insert(link));
                if !counted {
                    totals.disk = entry.disk;
                    totals.apparent = entry.apparent;
                }
                totals.files = 1;
                totals.by_category[0][category.index()] = totals.disk;
                totals.by_category[1][category.index()] = totals.apparent;
            } else if entry.kind == Kind::Symlink {
                totals.files = 1;
            }
            sum.add(&totals);
            newest = newest.max(entry.modified);
            let id = self.nodes.len() as u32;
            let first = self.nodes[parent as usize].first_child;
            if entry.kind == Kind::Dir {
                self.dirs += 1;
                dirs.push((id, parent_path.join(&entry.name)));
            }
            self.nodes.push(Node {
                name: entry.name.into_boxed_os_str(),
                parent,
                first_child: NONE,
                next_sibling: first,
                kind: entry.kind,
                totals,
                newest: entry.modified,
                unreadable: false,
            });
            self.nodes[parent as usize].first_child = id;
        }
        self.add_up(parent, &sum, newest);
        self.generation += 1;
        dirs
    }

    /// Takes a node out of the tree, and its sizes out of its folders'.
    pub fn remove(&mut self, id: u32) {
        let parent = self.node(id).parent;
        if parent == NONE {
            return;
        }
        let first = self.node(parent).first_child;
        let next = self.node(id).next_sibling;
        if first == id {
            self.nodes[parent as usize].first_child = next;
        } else {
            let prev = self
                .children(parent)
                .find(|&c| self.node(c).next_sibling == id);
            if let Some(prev) = prev {
                self.nodes[prev as usize].next_sibling = next;
            }
        }
        let totals = self.node(id).totals.negated();
        self.add_up(parent, &totals, 0);
        self.nodes[id as usize].parent = NONE;
        self.generation += 1;
    }

    /// Empties a folder, to scan it again.
    fn clear(&mut self, id: u32) {
        for child in self.children(id).collect::<Vec<_>>() {
            self.remove(child);
        }
        self.nodes[id as usize].unreadable = false;
    }
}

/// One folder entry, read without holding the tree's lock.
struct Scanned {
    name: OsString,
    kind: Kind,
    disk: u64,
    apparent: u64,
    modified: i64,
    link: Option<(u64, u64)>,
}

fn seconds(time: std::io::Result<SystemTime>) -> i64 {
    time.ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(unix)]
fn device(meta: &fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::dev(meta)
}

#[cfg(not(unix))]
fn device(_meta: &fs::Metadata) -> u64 {
    0
}

fn read(dir: &Path, root_device: u64) -> std::io::Result<Vec<Scanned>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        // Not followed: symlinks (junctions on Windows) only count themselves.
        let Ok(meta) = entry.metadata() else { continue };
        let name = entry.file_name();
        let modified = seconds(meta.modified());
        let kind = if meta.file_type().is_symlink() {
            Kind::Symlink
        } else if meta.is_dir() {
            if device(&meta) == root_device {
                Kind::Dir
            } else {
                Kind::Mount
            }
        } else {
            Kind::File(Category::of(&name))
        };
        #[cfg(unix)]
        let (disk, link) = {
            use std::os::unix::fs::MetadataExt;
            let link = (meta.nlink() > 1 && kind != Kind::Dir).then(|| (meta.dev(), meta.ino()));
            (meta.blocks() * 512, link)
        };
        #[cfg(not(unix))]
        let (disk, link) = (meta.len(), None);
        entries.push(Scanned {
            name,
            kind,
            disk: if kind == Kind::Dir { 0 } else { disk },
            apparent: if kind == Kind::Dir { 0 } else { meta.len() },
            modified,
            link,
        });
    }
    Ok(entries)
}

enum Work {
    /// List a folder that's empty in the tree, and everything below it.
    Scan(u32, PathBuf),
    /// List a folder again, keeping subfolders that are still there.
    Refresh(u32, PathBuf),
}

#[derive(Default)]
struct Queue {
    work: Vec<Work>,
    /// Workers listing a folder right now.
    busy: usize,
    /// Worker threads alive.
    workers: usize,
}

pub struct Scan {
    pub tree: RwLock<Tree>,
    queue: Mutex<Queue>,
    wake: Condvar,
    cancel: AtomicBool,
    root_device: u64,
    ctx: Context,
    /// When the scan started, and how long it took once it's done.
    timing: Mutex<(Instant, Option<Duration>)>,
}

impl Scan {
    pub fn start(ctx: &Context, root: PathBuf) -> Arc<Self> {
        let meta = fs::metadata(&root);
        let root_device = meta.as_ref().map(device).unwrap_or(0);
        let scan = Arc::new(Self {
            tree: RwLock::new(Tree::new(root.clone())),
            queue: Mutex::default(),
            wake: Condvar::new(),
            cancel: AtomicBool::new(false),
            root_device,
            ctx: ctx.clone(),
            timing: Mutex::new((Instant::now(), None)),
        });
        scan.push(vec![Work::Scan(ROOT, root)]);
        scan
    }

    pub fn tree(&self) -> std::sync::RwLockReadGuard<'_, Tree> {
        self.tree.read().unwrap()
    }

    pub fn running(&self) -> bool {
        let queue = self.queue.lock().unwrap();
        queue.workers > 0 || !queue.work.is_empty()
    }

    pub fn elapsed(&self) -> Duration {
        let (start, took) = *self.timing.lock().unwrap();
        took.unwrap_or_else(|| start.elapsed())
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        let mut queue = self.queue.lock().unwrap();
        queue.work.clear();
        self.wake.notify_all();
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Scans a folder again from scratch.
    pub fn rescan(self: &Arc<Self>, id: u32) {
        let path = {
            let mut tree = self.tree.write().unwrap();
            if !tree.is_attached(id) || !tree.node(id).is_dir() {
                return;
            }
            tree.clear(id);
            tree.path(id)
        };
        self.cancel.store(false, Ordering::Relaxed);
        self.restart_timing();
        self.push(vec![Work::Scan(id, path)]);
    }

    /// Notices what changed in a folder: new entries are added (new folders
    /// scanned), gone ones taken out, and files measured again.
    pub fn refresh(self: &Arc<Self>, id: u32) {
        let path = self.tree().path(id);
        self.push(vec![Work::Refresh(id, path)]);
    }

    fn restart_timing(&self) {
        let mut timing = self.timing.lock().unwrap();
        if timing.1.is_some() {
            *timing = (Instant::now(), None);
        }
    }

    fn push(self: &Arc<Self>, work: Vec<Work>) {
        if work.is_empty() {
            return;
        }
        let mut queue = self.queue.lock().unwrap();
        queue.work.extend(work);
        let wanted = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .clamp(2, 8);
        while queue.workers < wanted {
            queue.workers += 1;
            let scan = self.clone();
            let spawned = std::thread::Builder::new()
                .name("disk-usage".into())
                .spawn(move || scan.work());
            if spawned.is_err() {
                queue.workers -= 1;
                break;
            }
        }
        self.wake.notify_all();
    }

    fn work(&self) {
        loop {
            let job = {
                let mut queue = self.queue.lock().unwrap();
                loop {
                    if self.cancelled() {
                        break None;
                    }
                    if let Some(job) = queue.work.pop() {
                        queue.busy += 1;
                        break Some(job);
                    }
                    if queue.busy == 0 {
                        break None;
                    }
                    queue = self.wake.wait(queue).unwrap();
                }
            };
            let Some(job) = job else {
                let mut queue = self.queue.lock().unwrap();
                queue.workers -= 1;
                if queue.workers == 0 {
                    let mut timing = self.timing.lock().unwrap();
                    if timing.1.is_none() {
                        timing.1 = Some(timing.0.elapsed());
                    }
                }
                self.wake.notify_all();
                self.ctx.request_repaint();
                return;
            };
            let more = match job {
                Work::Scan(id, path) => self.scan_dir(id, &path),
                Work::Refresh(id, path) => self.refresh_dir(id, &path),
            };
            let mut queue = self.queue.lock().unwrap();
            queue.busy -= 1;
            queue.work.extend(more);
            self.wake.notify_all();
            drop(queue);
            self.ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn scan_dir(&self, id: u32, path: &Path) -> Vec<Work> {
        let read = read(path, self.root_device);
        let mut tree = self.tree.write().unwrap();
        match read {
            Ok(entries) => tree
                .add_children(id, entries)
                .into_iter()
                .map(|(id, path)| Work::Scan(id, path))
                .collect(),
            Err(_) => {
                tree.nodes[id as usize].unreadable = true;
                tree.unreadable += 1;
                tree.generation += 1;
                Vec::new()
            }
        }
    }

    fn refresh_dir(&self, id: u32, path: &Path) -> Vec<Work> {
        let Ok(entries) = read(path, self.root_device) else {
            return Vec::new();
        };
        let mut tree = self.tree.write().unwrap();
        if !tree.is_attached(id) {
            return Vec::new();
        }
        let names: HashSet<&OsStr> = entries.iter().map(|e| e.name.as_os_str()).collect();
        let mut kept = HashSet::new();
        for child in tree.children(id).collect::<Vec<_>>() {
            let node = tree.node(child);
            // Folders keep what's below them; files are measured again.
            if node.is_dir() && names.contains(&*node.name) {
                kept.insert(node.name.to_os_string());
            } else {
                tree.remove(child);
            }
        }
        let fresh = entries
            .into_iter()
            .filter(|e| !(e.kind == Kind::Dir && kept.contains(&e.name)))
            .collect();
        tree.add_children(id, fresh)
            .into_iter()
            .map(|(id, path)| Work::Scan(id, path))
            .collect()
    }
}

/// Owns a running scan: dropping it stops the workers.
pub struct ScanHandle(pub Arc<Scan>);

impl Drop for ScanHandle {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl std::ops::Deref for ScanHandle {
    type Target = Arc<Scan>;

    fn deref(&self) -> &Arc<Scan> {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(scan: &Scan) {
        let start = Instant::now();
        while scan.running() {
            assert!(start.elapsed() < Duration::from_secs(10), "scan hung");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "egui-files-usage-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::create_dir_all(dir.join("c")).unwrap();
        fs::write(dir.join("a/one.mp4"), vec![1; 10_000]).unwrap();
        fs::write(dir.join("a/b/two.rs"), vec![1; 3_000]).unwrap();
        fs::write(dir.join("c/three.txt"), vec![1; 500]).unwrap();
        dir
    }

    #[test]
    fn adds_up_sizes_and_categories() {
        let root = dir();
        let scan = Scan::start(&Context::default(), root.clone());
        wait(&scan);
        let tree = scan.tree();
        assert_eq!(tree.node(ROOT).totals.apparent, 13_500);
        assert_eq!(tree.node(ROOT).totals.files, 3);
        assert_eq!(tree.dirs, 4);
        let a = tree.find(&root.join("a")).unwrap();
        assert_eq!(tree.node(a).size(Metric::Apparent), 13_000);
        assert_eq!(
            tree.node(a)
                .totals
                .category(Metric::Apparent, Category::Video),
            10_000
        );
        assert_eq!(tree.node(a).category(Metric::Apparent), Category::Video);
        let two = tree.find(&root.join("a/b/two.rs")).unwrap();
        assert_eq!(tree.path(two), root.join("a/b/two.rs"));
        assert_eq!(tree.sorted_children(ROOT, Metric::Apparent)[0], a);
        drop(tree);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removes_rescans_and_refreshes() {
        let root = dir();
        let scan = Scan::start(&Context::default(), root.clone());
        wait(&scan);
        let a = scan.tree().find(&root.join("a")).unwrap();
        scan.tree.write().unwrap().remove(a);
        assert_eq!(scan.tree().node(ROOT).totals.apparent, 500);
        assert!(!scan.tree().is_attached(a));

        scan.rescan(ROOT);
        wait(&scan);
        assert_eq!(scan.tree().node(ROOT).totals.apparent, 13_500);

        fs::write(root.join("a/b/new.png"), vec![1; 2_000]).unwrap();
        fs::remove_file(root.join("a/b/two.rs")).unwrap();
        fs::create_dir(root.join("a/b/sub")).unwrap();
        fs::write(root.join("a/b/sub/deep.png"), vec![1; 100]).unwrap();
        let b = scan.tree().find(&root.join("a/b")).unwrap();
        scan.refresh(b);
        wait(&scan);
        let tree = scan.tree();
        assert_eq!(tree.node(ROOT).totals.apparent, 12_600);
        assert_eq!(tree.node(b).totals.files, 2);
        assert!(tree.find(&root.join("a/b/sub/deep.png")).is_some());
        drop(tree);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn counts_hard_links_once() {
        let root = dir();
        fs::hard_link(root.join("a/one.mp4"), root.join("c/again.mp4")).unwrap();
        let scan = Scan::start(&Context::default(), root.clone());
        wait(&scan);
        assert_eq!(scan.tree().node(ROOT).totals.apparent, 13_500);
        assert_eq!(scan.tree().node(ROOT).totals.files, 4);
        fs::remove_dir_all(root).unwrap();
    }
}
