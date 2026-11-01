//! Changes to files, as a plan that can be applied here or, after a
//! "permission denied", by an elevated copy of the app (see `elevate`).

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    time::SystemTime,
};

use serde::{Deserialize, Serialize};

use super::platform;

/// Bits to turn on and off, leaving the rest as they are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bits {
    pub set: u32,
    pub clear: u32,
}

impl Bits {
    pub fn apply(self, value: u32) -> u32 {
        (value & !self.clear) | self.set
    }

    pub fn is_empty(self) -> bool {
        self.set == 0 && self.clear == 0
    }

    /// Turns `bit` on or off.
    pub fn put(&mut self, bit: u32, on: bool) {
        if on {
            self.set |= bit;
            self.clear &= !bit;
        } else {
            self.clear |= bit;
            self.set &= !bit;
        }
    }

    /// What the change does to `bit`, if anything.
    pub fn get(self, bit: u32) -> Option<bool> {
        if self.set & bit != 0 {
            Some(true)
        } else if self.clear & bit != 0 {
            Some(false)
        } else {
            None
        }
    }
}

/// One change. With `recursive`, it also applies to everything inside a
/// folder; `files` and `dirs` say what to do to each kind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Op {
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    /// Unix permission bits.
    Mode {
        path: PathBuf,
        files: Bits,
        dirs: Bits,
        recursive: bool,
    },
    /// Windows file attributes.
    Attributes {
        path: PathBuf,
        files: Bits,
        dirs: Bits,
        recursive: bool,
    },
    /// Unix user and group ids, or a Windows account name.
    Owner {
        path: PathBuf,
        uid: Option<u32>,
        gid: Option<u32>,
        account: Option<String>,
        recursive: bool,
    },
    Times {
        path: PathBuf,
        modified: Option<SystemTime>,
        accessed: Option<SystemTime>,
    },
    /// Points a symlink somewhere else.
    Symlink {
        path: PathBuf,
        target: PathBuf,
    },
}

impl Op {
    pub fn path(&self) -> &Path {
        match self {
            Op::Rename { from, .. } => from,
            Op::Mode { path, .. }
            | Op::Attributes { path, .. }
            | Op::Owner { path, .. }
            | Op::Times { path, .. }
            | Op::Symlink { path, .. } => path,
        }
    }

    fn recursive(&self) -> bool {
        match self {
            Op::Mode { recursive, .. }
            | Op::Attributes { recursive, .. }
            | Op::Owner { recursive, .. } => *recursive,
            Op::Rename { .. } | Op::Times { .. } | Op::Symlink { .. } => false,
        }
    }

    /// The same change, for `path` only.
    fn at(&self, path: &Path) -> Op {
        let mut op = self.clone();
        match &mut op {
            Op::Rename { .. } | Op::Times { .. } | Op::Symlink { .. } => {}
            Op::Mode {
                path: p, recursive, ..
            }
            | Op::Attributes {
                path: p, recursive, ..
            }
            | Op::Owner {
                path: p, recursive, ..
            } => {
                *p = path.to_path_buf();
                *recursive = false;
            }
        }
        op
    }

    /// Does the change to the one path it names.
    fn apply_one(&self, path: &Path, top: bool) -> io::Result<()> {
        match self {
            Op::Rename { from, to } => {
                if to.symlink_metadata().is_ok() && !same_file_differing_case(from, to) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("“{}” already exists", crate::fs::display_name(to)),
                    ));
                }
                fs::rename(from, to)
            }
            Op::Mode { files, dirs, .. } => {
                let meta = if top {
                    fs::metadata(path)?
                } else {
                    fs::symlink_metadata(path)?
                };
                // chmod follows links; the link itself has no bits of its own.
                if meta.file_type().is_symlink() {
                    return Ok(());
                }
                let bits = if meta.is_dir() { dirs } else { files };
                if bits.is_empty() {
                    return Ok(());
                }
                let current = mode_of(&meta);
                let new = bits.apply(current);
                if new == current {
                    return Ok(());
                }
                platform::set_mode(path, new)
            }
            Op::Attributes { files, dirs, .. } => {
                let meta = fs::symlink_metadata(path)?;
                let bits = if meta.is_dir() { dirs } else { files };
                let current = attributes_of(&meta);
                let new = bits.apply(current);
                if bits.is_empty() || new == current {
                    return Ok(());
                }
                platform::set_attributes(path, new)
            }
            Op::Owner {
                uid, gid, account, ..
            } => platform::set_owner(path, *uid, *gid, account.as_deref(), top),
            Op::Times {
                modified, accessed, ..
            } => {
                let meta = fs::metadata(path)?;
                let modified = modified.or(meta.modified().ok());
                let accessed = accessed.or(meta.accessed().ok()).or(modified);
                match (accessed, modified) {
                    (Some(a), Some(m)) => filetime::set_file_times(
                        path,
                        filetime::FileTime::from_system_time(a),
                        filetime::FileTime::from_system_time(m),
                    ),
                    _ => Err(io::Error::other("the file's times can't be read")),
                }
            }
            Op::Symlink { path, target } => retarget(path, target),
        }
    }
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(_meta: &fs::Metadata) -> u32 {
    0
}

#[cfg(windows)]
fn attributes_of(meta: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes()
}

#[cfg(not(windows))]
fn attributes_of(_meta: &fs::Metadata) -> u32 {
    0
}

/// Renaming "a.txt" to "A.txt" on a case-insensitive filesystem: the new
/// name "exists" because it is the same file.
fn same_file_differing_case(from: &Path, to: &Path) -> bool {
    from.parent() == to.parent()
        && from.file_name().zip(to.file_name()).is_some_and(|(a, b)| {
            a != b && a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        })
}

#[cfg(unix)]
fn retarget(path: &Path, target: &Path) -> io::Result<()> {
    // Make the new link beside the old one and rename it over, so the link
    // never goes missing.
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = path.with_file_name(format!(".{name}.relink-{}", std::process::id()));
    std::os::unix::fs::symlink(target, &temp)?;
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

#[cfg(windows)]
fn retarget(path: &Path, target: &Path) -> io::Result<()> {
    let resolved = path
        .parent()
        .map_or(target.to_path_buf(), |p| p.join(target));
    let was_dir = fs::metadata(path).is_ok_and(|m| m.is_dir()) || resolved.is_dir();
    // Make the new link first: creating links needs a privilege that
    // removing doesn't, and failing then must leave the old one in place.
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = path.with_file_name(format!("{name}.relink-{}", std::process::id()));
    if was_dir {
        std::os::windows::fs::symlink_dir(target, &temp)?;
    } else {
        std::os::windows::fs::symlink_file(target, &temp)?;
    }
    let removed = if was_dir {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    };
    removed
        .and_then(|()| fs::rename(&temp, path))
        .inspect_err(|_| {
            let _ = if was_dir {
                fs::remove_dir(&temp)
            } else {
                fs::remove_file(&temp)
            };
        })
}

#[cfg(not(any(unix, windows)))]
fn retarget(_path: &Path, _target: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported"))
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub ops: Vec<Op>,
}

/// A change that didn't work, as an op for just that path, to retry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub op: Op,
    pub error: String,
    /// Failed for lack of permission, so retrying elevated may help.
    pub denied: bool,
}

/// The failures of a plan, as the elevated helper reports them.
#[derive(Default, Serialize, Deserialize)]
pub struct Report {
    pub failures: Vec<Failure>,
}

/// Shared with the thread applying a plan.
#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub cancel: AtomicBool,
}

/// Most failures kept; a recursive change on a big tree could fail for
/// every file in it.
const MAX_FAILURES: usize = 500;

/// Applies every op in order, carrying on past failures.
pub fn apply(plan: &Plan, progress: &Progress) -> Vec<Failure> {
    let mut failures = Vec::new();
    let mut fail = |op: &Op, path: &Path, error: io::Error| {
        if failures.len() < MAX_FAILURES {
            failures.push(Failure {
                op: op.at(path),
                denied: error.kind() == io::ErrorKind::PermissionDenied,
                error: format!("{}: {error}", path.display()),
            });
        }
    };
    for op in &plan.ops {
        if progress.cancel.load(Relaxed) {
            break;
        }
        let path = op.path().to_path_buf();
        if let Err(error) = op.apply_one(&path, true) {
            fail(op, &path, error);
        }
        progress.done.fetch_add(1, Relaxed);
        if op.recursive() && fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            let mut stack = vec![path];
            while let Some(dir) = stack.pop() {
                if progress.cancel.load(Relaxed) {
                    break;
                }
                let entries = match fs::read_dir(&dir) {
                    Ok(entries) => entries,
                    Err(error) => {
                        fail(op, &dir, error);
                        continue;
                    }
                };
                for entry in entries.flatten() {
                    let child = entry.path();
                    if let Err(error) = op.apply_one(&child, false) {
                        fail(op, &child, error);
                    }
                    progress.done.fetch_add(1, Relaxed);
                    // Not into linked folders: they may be anywhere.
                    if entry.file_type().is_ok_and(|t| t.is_dir()) {
                        stack.push(child);
                    }
                }
            }
        }
    }
    failures
}

/// Applies `plan` on a background thread.
pub struct Applying {
    pub progress: Arc<Progress>,
    result: Arc<std::sync::Mutex<Option<Vec<Failure>>>>,
}

impl Applying {
    pub fn start(ctx: &egui::Context, plan: Plan) -> Self {
        let progress = Arc::new(Progress::default());
        let result = Arc::new(std::sync::Mutex::new(None));
        let (p, r, ctx) = (progress.clone(), result.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("properties-apply".into())
            .spawn(move || {
                let failures = apply(&plan, &p);
                *r.lock().unwrap() = Some(failures);
                ctx.request_repaint();
            })
            .expect("spawn thread");
        Self { progress, result }
    }

    /// The failures, once done.
    pub fn take(&self) -> Option<Vec<Failure>> {
        self.result.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("egui-files-props-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bits_turn_on_and_off() {
        let mut bits = Bits::default();
        bits.put(0o100, true);
        bits.put(0o002, false);
        assert_eq!(bits.apply(0o646), 0o744);
        assert_eq!(bits.get(0o100), Some(true));
        assert_eq!(bits.get(0o002), Some(false));
        assert_eq!(bits.get(0o004), None);
        bits.put(0o100, false);
        assert_eq!(bits.apply(0o746), 0o644);
    }

    #[test]
    fn renames_and_refuses_to_overwrite() {
        let dir = temp("rename");
        fs::write(dir.join("a"), "a").unwrap();
        fs::write(dir.join("b"), "b").unwrap();
        let plan = Plan {
            ops: vec![Op::Rename {
                from: dir.join("a"),
                to: dir.join("b"),
            }],
        };
        let failures = apply(&plan, &Progress::default());
        assert_eq!(failures.len(), 1);
        assert_eq!(fs::read_to_string(dir.join("b")).unwrap(), "b");
        let plan = Plan {
            ops: vec![Op::Rename {
                from: dir.join("a"),
                to: dir.join("c"),
            }],
        };
        assert!(apply(&plan, &Progress::default()).is_empty());
        assert!(dir.join("c").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn recursive_modes_treat_files_and_folders_apart() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("mode");
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/file"), "x").unwrap();
        fs::set_permissions(dir.join("sub/file"), fs::Permissions::from_mode(0o600)).unwrap();
        let mode = |p: &str| fs::metadata(dir.join(p)).unwrap().permissions().mode() & 0o777;
        let plan = Plan {
            ops: vec![Op::Mode {
                path: dir.join("sub"),
                files: Bits {
                    set: 0o044,
                    clear: 0o111,
                },
                dirs: Bits {
                    set: 0o055,
                    clear: 0,
                },
                recursive: true,
            }],
        };
        assert!(apply(&plan, &Progress::default()).is_empty());
        assert_eq!(mode("sub/file"), 0o644);
        assert_eq!(mode("sub") & 0o055, 0o055);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sets_times() {
        let dir = temp("times");
        let file = dir.join("f");
        fs::write(&file, "x").unwrap();
        let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        let plan = Plan {
            ops: vec![Op::Times {
                path: file.clone(),
                modified: Some(when),
                accessed: None,
            }],
        };
        assert!(apply(&plan, &Progress::default()).is_empty());
        assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), when);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn retargets_links() {
        let dir = temp("link");
        std::os::unix::fs::symlink("old", dir.join("l")).unwrap();
        let plan = Plan {
            ops: vec![Op::Symlink {
                path: dir.join("l"),
                target: PathBuf::from("new"),
            }],
        };
        assert!(apply(&plan, &Progress::default()).is_empty());
        assert_eq!(fs::read_link(dir.join("l")).unwrap(), PathBuf::from("new"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn plans_survive_toml() {
        let plan = Plan {
            ops: vec![
                Op::Owner {
                    path: PathBuf::from("/a b"),
                    uid: Some(1000),
                    gid: None,
                    account: None,
                    recursive: true,
                },
                Op::Times {
                    path: PathBuf::from("/c"),
                    modified: Some(SystemTime::UNIX_EPOCH),
                    accessed: None,
                },
            ],
        };
        let text = toml::to_string(&plan).unwrap();
        assert_eq!(toml::from_str::<Plan>(&text).unwrap(), plan);
    }
}
