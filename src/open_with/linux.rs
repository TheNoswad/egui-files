//! Apps from freedesktop desktop entries, file types from the shared MIME
//! database, and associations from `mimeapps.list`.
//!
//! Specs: <https://specifications.freedesktop.org/desktop-entry-spec/latest/>
//! and <https://specifications.freedesktop.org/mime-apps-spec/latest/>.

use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsString,
    io,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::SystemTime,
};

use xdg_mime::SharedMimeInfo;

use super::{App, Handlers, Launch, Subject, Target};

/// Everything read from disk to answer "which apps open this".
pub struct Catalog {
    mime: SharedMimeInfo,
    /// Every installed app, by desktop file id.
    apps: HashMap<String, Entry>,
    /// Apps in the order "all apps" lists them.
    sorted: Vec<String>,
    /// Defaults per type from `mimeapps.list`, most important first.
    defaults: HashMap<String, Vec<String>>,
    /// Apps per type: added associations first, then desktop entries that
    /// declare the type, minus removed associations.
    associated: HashMap<String, Vec<String>>,
    /// When each file read was last changed, to notice edits.
    stamps: Vec<(PathBuf, Option<SystemTime>)>,
}

struct Entry {
    app: App,
    exec: String,
    /// The `.desktop` file, for `%k`.
    file: PathBuf,
    /// `Path=`: the folder to start the app in.
    dir: Option<PathBuf>,
    /// Hidden from app lists (`NoDisplay`, or not shown in this desktop),
    /// but still offered for the types it handles.
    no_display: bool,
}

fn data_dirs() -> Vec<PathBuf> {
    let home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|d| d.data_dir().to_path_buf()));
    let dirs = env::var_os("XDG_DATA_DIRS")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| OsString::from("/usr/local/share:/usr/share"));
    home.into_iter().chain(env::split_paths(&dirs)).collect()
}

fn config_dirs() -> Vec<PathBuf> {
    let home = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|d| d.config_dir().to_path_buf()));
    let dirs = env::var_os("XDG_CONFIG_DIRS")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| OsString::from("/etc/xdg"));
    home.into_iter().chain(env::split_paths(&dirs)).collect()
}

/// `XDG_CURRENT_DESKTOP`, e.g. `["KDE"]`.
fn desktops() -> Vec<String> {
    env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The `mimeapps.list` files in order of precedence.
fn mimeapps_files() -> Vec<PathBuf> {
    let desktops: Vec<String> = desktops().iter().map(|d| d.to_lowercase()).collect();
    let config = config_dirs();
    let data: Vec<PathBuf> = data_dirs().iter().map(|d| d.join("applications")).collect();
    let mut files = Vec::new();
    for dir in config.iter().chain(&data) {
        for desktop in &desktops {
            files.push(dir.join(format!("{desktop}-mimeapps.list")));
        }
        files.push(dir.join("mimeapps.list"));
    }
    files
}

fn modified(path: &Path) -> Option<SystemTime> {
    disk::metadata(path).and_then(|m| m.modified()).ok()
}

/// The groups of an ini-style file, keeping the order of keys.
fn parse_ini(text: &str) -> Vec<(&str, Vec<(&str, &str)>)> {
    let mut groups: Vec<(&str, Vec<(&str, &str)>)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            groups.push((name, Vec::new()));
        } else if let Some((key, value)) = line.split_once('=')
            && let Some((_, keys)) = groups.last_mut()
        {
            keys.push((key.trim(), value.trim()));
        }
    }
    groups
}

/// Undoes the escapes of a desktop entry string value.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn list(value: &str) -> impl Iterator<Item = &str> {
    value.split(';').map(str::trim).filter(|s| !s.is_empty())
}

/// The locale suffixes to try for localized keys, best first:
/// `de_AT@euro` gives `de_AT@euro`, `de_AT`, `de@euro`, `de`.
fn locales() -> Vec<String> {
    let lang = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|v| env::var(v).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default();
    let lang = lang.split('.').next().unwrap_or_default();
    let (base, modifier) = match lang.split_once('@') {
        Some((base, modifier)) => (base, Some(modifier)),
        None => (lang, None),
    };
    let (language, country) = match base.split_once('_') {
        Some((language, country)) => (language, Some(country)),
        None => (base, None),
    };
    if language.is_empty() || language == "C" || language == "POSIX" {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let (Some(country), Some(modifier)) = (country, modifier) {
        out.push(format!("{language}_{country}@{modifier}"));
    }
    if let Some(country) = country {
        out.push(format!("{language}_{country}"));
    }
    if let Some(modifier) = modifier {
        out.push(format!("{language}@{modifier}"));
    }
    out.push(language.to_owned());
    out
}

/// Whether the start of the file is UTF-8 text without control characters.
fn looks_like_text(path: &Path) -> bool {
    use std::io::Read;
    let mut buf = [0; 4096];
    let Ok(n) = disk::File::open(path).and_then(|mut f| f.read(&mut buf)) else {
        return false;
    };
    let text = match std::str::from_utf8(&buf[..n]) {
        Ok(text) => text,
        // Cut off mid-character at the end of the buffer.
        Err(err) if err.error_len().is_none() => {
            std::str::from_utf8(&buf[..err.valid_up_to()]).unwrap_or_default()
        }
        Err(_) => return false,
    };
    !text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\x0c'))
}

/// Whether `program` names a file that can be run, directly or in `PATH`.
fn find_program(program: &str) -> bool {
    let path = Path::new(program);
    if path.is_absolute() {
        return path.is_file();
    }
    env::var_os("PATH")
        .is_some_and(|dirs| env::split_paths(&dirs).any(|dir| dir.join(program).is_file()))
}

fn parse_entry(id: String, file: &Path, locales: &[String], desktops: &[String]) -> Option<Entry> {
    let text = disk::read_to_string(file).ok()?;
    let groups = parse_ini(&text);
    let (_, keys) = groups.iter().find(|(name, _)| *name == "Desktop Entry")?;
    let get = |key: &str| keys.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
    let localized = |key: &str| {
        locales
            .iter()
            .find_map(|l| get(&format!("{key}[{l}]")))
            .or_else(|| get(key))
            .map(unescape)
    };
    let yes = |key: &str| get(key) == Some("true");
    if get("Type") != Some("Application") || yes("Hidden") {
        return None;
    }
    let exec = unescape(get("Exec")?);
    if let Some(try_exec) = get("TryExec")
        && !find_program(&unescape(try_exec))
    {
        return None;
    }
    let shown_here = |key: &str| {
        get(key).map(|v| list(v).any(|d| desktops.iter().any(|ours| ours.eq_ignore_ascii_case(d))))
    };
    let no_display = yes("NoDisplay")
        || shown_here("OnlyShowIn") == Some(false)
        || shown_here("NotShowIn") == Some(true);
    let mime_types = get("MimeType")
        .map(|v| list(v).map(str::to_owned).collect())
        .unwrap_or_default();
    Some(Entry {
        app: App {
            name: localized("Name").unwrap_or_else(|| id.trim_end_matches(".desktop").to_owned()),
            comment: localized("GenericName")
                .or_else(|| localized("Comment"))
                .unwrap_or_default(),
            icon: get("Icon").map(unescape).filter(|i| !i.is_empty()),
            terminal: yes("Terminal"),
            mime_types,
            id,
        },
        exec,
        file: file.to_path_buf(),
        dir: get("Path")
            .map(unescape)
            .filter(|p| !p.is_empty())
            .map(PathBuf::from),
        no_display,
    })
}

/// Desktop files under `dir`, with their ids: the path relative to
/// `applications`, with `/` turned into `-`.
fn desktop_files(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = disk::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            desktop_files(&path, &format!("{prefix}{name}-"), out);
        } else if name.ends_with(".desktop") {
            out.push((format!("{prefix}{name}"), path));
        }
    }
}

impl Catalog {
    pub fn load() -> Self {
        let mime = SharedMimeInfo::new();
        let locales = locales();
        let desktops = desktops();
        let mut stamps = Vec::new();

        let mut apps: HashMap<String, Entry> = HashMap::new();
        for dir in data_dirs() {
            let dir = dir.join("applications");
            stamps.push((dir.clone(), modified(&dir)));
            let mut files = Vec::new();
            desktop_files(&dir, "", &mut files);
            for (id, file) in files {
                // Earlier folders win, even if their entry is hidden.
                if apps.contains_key(&id) {
                    continue;
                }
                if let Some(mut entry) = parse_entry(id.clone(), &file, &locales, &desktops) {
                    for mime_type in &mut entry.app.mime_types {
                        if let Ok(parsed) = mime_type.parse()
                            && let Some(real) = mime.unalias_mime_type(&parsed)
                        {
                            *mime_type = real.to_string();
                        }
                    }
                    apps.insert(id, entry);
                } else {
                    // Shadows entries of the same id further down.
                    apps.insert(id.clone(), Entry::placeholder(id));
                }
            }
        }
        apps.retain(|_, e| !e.exec.is_empty());

        let unalias = |t: &str| -> String {
            t.parse()
                .ok()
                .and_then(|m| mime.unalias_mime_type(&m))
                .map_or_else(|| t.to_owned(), |m| m.to_string())
        };
        let mut defaults: HashMap<String, Vec<String>> = HashMap::new();
        let mut added: HashMap<String, Vec<String>> = HashMap::new();
        let mut removed: HashSet<(String, String)> = HashSet::new();
        for file in mimeapps_files() {
            stamps.push((file.clone(), modified(&file)));
            let Ok(text) = disk::read_to_string(&file) else {
                continue;
            };
            let mut removed_here = Vec::new();
            for (group, keys) in parse_ini(&text) {
                for (mime_type, ids) in keys {
                    let mime_type = unalias(mime_type);
                    let ids = list(ids).filter(|id| apps.contains_key(*id));
                    match group {
                        "Default Applications" => defaults
                            .entry(mime_type)
                            .or_default()
                            .extend(ids.map(str::to_owned)),
                        "Added Associations" => {
                            let list = added.entry(mime_type.clone()).or_default();
                            for id in ids {
                                // Removals in more important files win.
                                if !removed.contains(&(mime_type.clone(), id.to_owned())) {
                                    list.push(id.to_owned());
                                }
                            }
                        }
                        "Removed Associations" => {
                            removed_here.extend(ids.map(|id| (mime_type.clone(), id.to_owned())))
                        }
                        _ => {}
                    }
                }
            }
            removed.extend(removed_here);
        }

        let mut sorted: Vec<String> = apps.keys().cloned().collect();
        sorted.sort_by_cached_key(|id| (apps[id].app.name.to_lowercase(), id.clone()));
        let mut associated = added;
        for id in &sorted {
            for mime_type in &apps[id].app.mime_types {
                if !removed.contains(&(mime_type.clone(), id.clone())) {
                    associated
                        .entry(mime_type.clone())
                        .or_default()
                        .push(id.clone());
                }
            }
        }
        for ids in associated.values_mut() {
            let mut seen = HashSet::new();
            ids.retain(|id| seen.insert(id.clone()));
        }
        Self {
            mime,
            apps,
            sorted,
            defaults,
            associated,
            stamps,
        }
    }

    /// Whether any file read when loading changed since.
    pub fn is_stale(&self) -> bool {
        self.stamps
            .iter()
            .any(|(path, when)| modified(path) != *when)
    }

    /// The file's type, sniffing its contents when the name isn't enough.
    fn mime_type(&self, subject: &Subject) -> String {
        if subject.on_disk && subject.path.is_dir() {
            return "inode/directory".to_owned();
        }
        let name = subject
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let mut by_name = self.mime.get_mime_types_from_file_name(&name);
        // What it says when no name pattern matched.
        by_name.retain(|m| m.essence_str() != "application/octet-stream");
        if by_name.len() == 1 || !subject.on_disk {
            if let Some(first) = by_name.first() {
                return first.to_string();
            }
            return "application/octet-stream".to_owned();
        }
        let guess = self.mime.guess_mime_type().path(&subject.path).guess();
        let guessed = guess.mime_type().to_string();
        // Like GLib, call files with no known type but readable text plain text.
        match guessed.as_str() {
            "application/x-zerosize" => "text/plain".to_owned(),
            "application/octet-stream" if looks_like_text(&subject.path) => "text/plain".to_owned(),
            _ => guessed,
        }
    }

    /// The type and everything it's a kind of, closest first.
    fn with_parents(&self, mime_type: &str) -> Vec<String> {
        let mut out = vec![mime_type.to_owned()];
        let mut i = 0;
        while i < out.len() {
            let parents = out[i]
                .parse()
                .ok()
                .and_then(|m| self.mime.get_parents(&m))
                .unwrap_or_default();
            for parent in parents {
                let parent = parent.to_string();
                // Too broad to be useful: every file is one.
                if parent != "application/octet-stream" && !out.contains(&parent) {
                    out.push(parent);
                }
            }
            i += 1;
        }
        if out[0].starts_with("text/") && !out.iter().any(|t| t == "text/plain") {
            out.push("text/plain".to_owned());
        }
        out
    }

    /// The default app for one type, and the others that open it.
    fn handlers_for_type(&self, mime_type: &str) -> (Option<String>, Vec<String>) {
        let types = self.with_parents(mime_type);
        let default = types
            .iter()
            .find_map(|t| {
                self.defaults
                    .get(t)?
                    .iter()
                    .find(|id| self.apps.contains_key(*id))
            })
            .cloned();
        let mut all: Vec<String> = Vec::new();
        for t in &types {
            for id in self.associated.get(t).into_iter().flatten() {
                if !all.contains(id) {
                    all.push(id.clone());
                }
            }
        }
        let default = default.or_else(|| all.first().cloned());
        (default, all)
    }

    pub fn handlers(&self, subjects: &[Subject]) -> Handlers {
        let mut types: Vec<String> = Vec::new();
        for subject in subjects {
            let t = self.mime_type(subject);
            if !types.contains(&t) {
                types.push(t);
            }
        }
        let mut per_type = types.iter().map(|t| self.handlers_for_type(t));
        let Some((mut default, mut common)) = per_type.next() else {
            return Handlers::default();
        };
        for (other_default, apps) in per_type {
            if other_default != default {
                default = None;
            }
            common.retain(|id| apps.contains(id));
        }
        let app = |id: &String| self.apps.get(id).map(|e| e.app.clone());
        Handlers {
            default: default.as_ref().and_then(app),
            others: common
                .iter()
                .filter(|id| Some(*id) != default.as_ref())
                .filter_map(app)
                .collect(),
            types,
        }
    }

    /// Every app shown in app menus, by name.
    pub fn all_apps(&self, _subjects: &[Subject]) -> Vec<App> {
        self.sorted
            .iter()
            .map(|id| &self.apps[id])
            .filter(|e| !e.no_display)
            .map(|e| e.app.clone())
            .collect()
    }

    pub fn launch(&self, target: &Target, paths: &[PathBuf]) -> io::Result<Vec<Launch>> {
        let (exec, entry) = match target {
            Target::App(app) => {
                let entry = self.apps.get(&app.id).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("{} is no longer installed", app.name),
                    )
                })?;
                (entry.exec.as_str(), Some(entry))
            }
            Target::Command { command, .. } => (command.as_str(), None),
        };
        let words = split_exec(exec)?;
        let terminal = match target {
            Target::App(app) => app.terminal,
            Target::Command { terminal, .. } => *terminal,
        };
        let commands = expand(&words, paths, entry, entry.is_none());
        let dir = entry.and_then(|e| e.dir.clone()).or_else(|| {
            paths
                .first()
                .and_then(|p| p.parent())
                .map(Path::to_path_buf)
        });
        let mut launches = Vec::new();
        for mut args in commands {
            if args.is_empty() {
                continue;
            }
            let program = args.remove(0);
            let launch = Launch {
                program,
                args,
                dir: dir.clone(),
            };
            if terminal {
                launches.push(launch);
            } else {
                spawn(&launch)?;
            }
        }
        Ok(launches)
    }
}

impl Entry {
    /// Marks an id as taken by an entry that can't be used.
    fn placeholder(id: String) -> Self {
        Self {
            app: App {
                id,
                ..App::default()
            },
            exec: String::new(),
            file: PathBuf::new(),
            dir: None,
            no_display: true,
        }
    }
}

/// The icon theme the desktop uses: KDE's, else GTK's.
fn icon_theme() -> String {
    let kde = config_dirs().into_iter().find_map(|dir| {
        let text = disk::read_to_string(dir.join("kdeglobals")).ok()?;
        let groups = parse_ini(&text);
        let (_, keys) = groups.into_iter().find(|(name, _)| *name == "Icons")?;
        keys.into_iter()
            .find(|(k, _)| *k == "Theme")
            .map(|(_, v)| v.to_owned())
    });
    let gtk = || {
        config_dirs().into_iter().find_map(|dir| {
            let text = disk::read_to_string(dir.join("gtk-3.0/settings.ini")).ok()?;
            text.lines()
                .find_map(|l| l.trim().strip_prefix("gtk-icon-theme-name"))
                .and_then(|v| v.trim_start().strip_prefix('='))
                .map(|v| v.trim().trim_matches('"').to_owned())
        })
    };
    kde.or_else(gtk)
        .or_else(freedesktop_icons::default_theme_gtk)
        .unwrap_or_else(|| "hicolor".to_owned())
}

/// An app's icon, `size` pixels square: `icon` is a name in the icon theme
/// or a path.
pub fn icon_image(icon: &str, size: u32) -> Option<egui::ColorImage> {
    static THEME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let path = if Path::new(icon).is_absolute() {
        PathBuf::from(icon)
    } else {
        let theme = THEME.get_or_init(icon_theme);
        freedesktop_icons::lookup(icon)
            .with_size(size as u16)
            .with_theme(theme)
            .with_cache()
            .find()?
    };
    let data = disk::read(&path).ok()?;
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg") || e.eq_ignore_ascii_case("svgz"))
    {
        let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).ok()?;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;
        let svg = tree.size();
        let scale = size as f32 / svg.width().max(svg.height());
        let transform = resvg::tiny_skia::Transform::from_scale(scale, scale).post_translate(
            (size as f32 - svg.width() * scale) / 2.0,
            (size as f32 - svg.height() * scale) / 2.0,
        );
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        Some(egui::ColorImage::from_rgba_premultiplied(
            [size as usize; 2],
            pixmap.data(),
        ))
    } else {
        let image = image::load_from_memory(&data).ok()?;
        let image = image
            .resize(size, size, image::imageops::FilterType::Triangle)
            .into_rgba8();
        Some(egui::ColorImage::from_rgba_unmultiplied(
            [image.width() as usize, image.height() as usize],
            image.as_raw(),
        ))
    }
}

/// Starts a program in its own session, so it outlives us and isn't stopped
/// by signals meant for us.
pub fn spawn(launch: &Launch) -> io::Result<()> {
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = launch.dir.as_ref().filter(|d| d.is_dir()) {
        command.current_dir(dir);
    }
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    // Reaped in the background, so it doesn't linger as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Splits an `Exec` value (already unescaped as a string) into arguments,
/// following its quoting rules.
pub fn split_exec(exec: &str) -> io::Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c) => word.push(c),
                            None => break,
                        },
                        Some(c) => word.push(c),
                        None => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "unterminated quote in command",
                            ));
                        }
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

fn file_url(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    const SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'/')
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    format!(
        "file://{}",
        percent_encoding::percent_encode(path.as_os_str().as_bytes(), SET)
    )
}

/// The command lines to run for `paths`: one, or one per file when the app
/// takes a single file (`%f`, `%u`). Without a file field code, an app gets
/// no files (as the spec says), but a custom command gets them appended.
fn expand(
    words: &[String],
    paths: &[PathBuf],
    entry: Option<&Entry>,
    append_files: bool,
) -> Vec<Vec<String>> {
    let path_str = |p: &PathBuf| p.to_string_lossy().into_owned();
    let has = |code: &str| words.iter().any(|w| w.contains(code));
    let single = has("%f") || has("%u");
    let multiple = words.iter().any(|w| w == "%F" || w == "%U");
    let groups: Vec<&[PathBuf]> = if single && !multiple && paths.len() > 1 {
        paths.chunks(1).collect()
    } else {
        vec![paths]
    };
    groups
        .into_iter()
        .map(|group| {
            let mut args = Vec::new();
            for word in words {
                match word.as_str() {
                    "%F" => args.extend(group.iter().map(path_str)),
                    "%U" => args.extend(group.iter().map(|p| file_url(p))),
                    "%i" => {
                        if let Some(icon) = entry.and_then(|e| e.app.icon.as_ref()) {
                            args.push("--icon".to_owned());
                            args.push(icon.clone());
                        }
                    }
                    _ => {
                        let mut out = String::new();
                        let mut chars = word.chars();
                        let mut dropped = false;
                        while let Some(c) = chars.next() {
                            if c != '%' {
                                out.push(c);
                                continue;
                            }
                            match chars.next() {
                                Some('%') => out.push('%'),
                                Some('f') => match group.first() {
                                    Some(p) => out.push_str(&path_str(p)),
                                    None => dropped = true,
                                },
                                Some('u') => match group.first() {
                                    Some(p) => out.push_str(&file_url(p)),
                                    None => dropped = true,
                                },
                                Some('c') => {
                                    out.push_str(entry.map_or("", |e| e.app.name.as_str()))
                                }
                                Some('k') => out.push_str(
                                    &entry.map(|e| e.file.to_string_lossy()).unwrap_or_default(),
                                ),
                                // Deprecated or unknown codes are dropped.
                                _ => {}
                            }
                        }
                        if !(dropped && out.is_empty()) {
                            args.push(out);
                        }
                    }
                }
            }
            if append_files && !single && !multiple {
                args.extend(group.iter().map(path_str));
            }
            args
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        split_exec(s).unwrap()
    }

    #[test]
    fn splits_quoted_exec() {
        assert_eq!(
            words(r#"foo "a b" c\d "q\"x\\y" %F"#),
            ["foo", "a b", r"c\d", r#"q"x\y"#, "%F"]
        );
        assert!(split_exec(r#"foo "open"#).is_err());
    }

    #[test]
    fn expands_field_codes() {
        let paths = [PathBuf::from("/a b/x.txt"), PathBuf::from("/y")];
        assert_eq!(
            expand(&words("app %F"), &paths, None, false),
            [vec!["app", "/a b/x.txt", "/y"]]
        );
        assert_eq!(
            expand(&words("app --file=%f"), &paths, None, false),
            [vec!["app", "--file=/a b/x.txt"], vec!["app", "--file=/y"]]
        );
        assert_eq!(
            expand(&words("app %U"), &paths[..1], None, false),
            [vec!["app", "file:///a%20b/x.txt"]]
        );
        assert_eq!(
            expand(&words("app 100%%"), &paths, None, false),
            [vec!["app", "100%"]]
        );
        // An app without a file code gets no files; a custom command does.
        assert_eq!(expand(&words("app"), &paths, None, false), [vec!["app"]]);
        assert_eq!(
            expand(&words("cmd -x"), &paths, None, true),
            [vec!["cmd", "-x", "/a b/x.txt", "/y"]]
        );
    }

    #[test]
    fn unescapes_and_localizes() {
        assert_eq!(unescape(r"a\sb\\c"), r"a b\c");
        let dir = std::env::temp_dir().join(format!("egui-files-desktop-{}", std::process::id()));
        disk::create_dir_all(&dir).unwrap();
        let file = dir.join("t.desktop");
        disk::write(
            &file,
            "[Desktop Entry]\nType=Application\nName=Viewer\nName[de]=Betrachter\n\
             Exec=view %f\nMimeType=image/png;text/plain;\nNoDisplay=true\n\
             [Desktop Action new]\nName=Other\n",
        )
        .unwrap();
        let entry = parse_entry(
            "t.desktop".into(),
            &file,
            &["de_DE".into(), "de".into()],
            &[],
        )
        .unwrap();
        assert_eq!(entry.app.name, "Betrachter");
        assert_eq!(entry.app.mime_types, ["image/png", "text/plain"]);
        assert!(entry.no_display);
        disk::remove_dir_all(&dir).unwrap();
    }

    /// Uses the system's MIME database and apps.
    #[test]
    fn finds_handlers_on_this_system() {
        let catalog = Catalog::load();
        let rust = Subject {
            path: PathBuf::from("/nonexistent/main.rs"),
            on_disk: false,
        };
        assert_eq!(catalog.mime_type(&rust), "text/rust");
        assert!(
            catalog
                .with_parents("text/rust")
                .contains(&"text/plain".to_owned())
        );
        let dir = Subject {
            path: PathBuf::from("/tmp"),
            on_disk: true,
        };
        assert_eq!(catalog.handlers(&[dir]).types, ["inode/directory"]);
    }
}

/// Makes `app` the default for `mime_type`, in the user's most important
/// `mimeapps.list` (a desktop-specific one if it exists, as it would win).
pub fn set_default(app: &App, mime_type: &str) -> io::Result<()> {
    let home = config_dirs()
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::other("no config folder"))?;
    let file = desktops()
        .iter()
        .map(|d| home.join(format!("{}-mimeapps.list", d.to_lowercase())))
        .find(|f| f.is_file())
        .unwrap_or_else(|| home.join("mimeapps.list"));
    let text = disk::read_to_string(&file).unwrap_or_default();
    let updated = with_default(&text, mime_type, &app.id);
    disk::create_dir_all(&home)?;
    disk::write(&file, updated)
}

/// `text` (a `mimeapps.list`) with `id` as the default for `mime_type`,
/// keeping everything else as it was.
fn with_default(text: &str, mime_type: &str, id: &str) -> String {
    const GROUP: &str = "[Default Applications]";
    let entry = format!("{mime_type}={id};");
    let mut out = Vec::new();
    let (mut in_group, mut seen_group, mut written) = (false, false, false);
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if in_group && !written {
                out.push(entry.clone());
                written = true;
            }
            in_group = trimmed == GROUP;
            seen_group |= in_group;
        } else if in_group
            && let Some((key, _)) = trimmed.split_once('=')
            && key.trim() == mime_type
        {
            if !written {
                out.push(entry.clone());
                written = true;
            }
            continue;
        }
        out.push(line.to_owned());
    }
    if !written {
        if !seen_group {
            if out.last().is_some_and(|l| !l.trim().is_empty()) {
                out.push(String::new());
            }
            out.push(GROUP.to_owned());
        }
        out.push(entry);
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod default_tests {
    use super::with_default;

    #[test]
    fn sets_a_default_keeping_the_rest() {
        assert_eq!(
            with_default("", "text/plain", "a.desktop"),
            "[Default Applications]\ntext/plain=a.desktop;\n"
        );
        let text = "[Added Associations]\ntext/plain=b.desktop;\n\n[Default Applications]\nimage/png=c.desktop;\ntext/plain=old.desktop;\n";
        assert_eq!(
            with_default(text, "text/plain", "a.desktop"),
            "[Added Associations]\ntext/plain=b.desktop;\n\n[Default Applications]\nimage/png=c.desktop;\ntext/plain=a.desktop;\n"
        );
        let text = "[Default Applications]\nimage/png=c.desktop;\n[Removed Associations]\n";
        assert_eq!(
            with_default(text, "text/plain", "a.desktop"),
            "[Default Applications]\nimage/png=c.desktop;\ntext/plain=a.desktop;\n[Removed Associations]\n"
        );
    }
}
