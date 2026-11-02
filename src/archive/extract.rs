//! The extract job: unpack into a hidden staging folder next to the
//! destination, then move the results into place, asking about conflicts.

use std::{
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use super::{Error, ExtractJob, ExtractMode, open_dir, read, stem};
use crate::{
    fs::unique_path,
    jobs::{self, Answer, ConflictChoice, Outcome, Progress, Prompt, Resolver},
};

/// Files opened from archives are kept this long, in case the app that opened them
/// still has them open.
const KEEP_OPENED: Duration = Duration::from_secs(24 * 60 * 60);

/// Removes the staging folder however the job ends.
struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn extract(job: ExtractJob, p: &Progress) {
    let ExtractJob {
        archive,
        dest,
        mode,
        selection,
        mut password,
        trash_after,
        open_with,
    } = job;
    let dest = if mode == ExtractMode::Open {
        clean_opened();
        unique_path(&open_dir(), OsStr::new(&stem(&archive)))
    } else {
        dest
    };
    if let Err(err) = fs::create_dir_all(&dest) {
        return p.error(&dest, err);
    }
    let staging = Staging(unique_path(
        &dest,
        OsStr::new(&format!(".{}.extracting", stem(&archive))),
    ));

    loop {
        if let Err(err) = fs::create_dir(&staging.0) {
            return p.error(&staging.0, err);
        }
        match read::unpack(
            &archive,
            &staging.0,
            selection.as_ref(),
            password.as_deref(),
            p,
        ) {
            Ok(()) => break,
            Err(err @ (Error::NeedsPassword | Error::WrongPassword)) => {
                let wrong = matches!(err, Error::WrongPassword);
                let Some(Answer::Password(answer)) = p.ask(Prompt::Password {
                    archive: archive.clone(),
                    wrong,
                }) else {
                    return;
                };
                password = Some(answer);
                // Start over with the password.
                let _ = fs::remove_dir_all(&staging.0);
                p.reset();
            }
            Err(Error::Cancelled) => return,
            Err(Error::Other(err)) => return p.error(&archive, err),
        }
    }

    let children: Vec<PathBuf> = match fs::read_dir(&staging.0) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
        Err(err) => return p.error(&staging.0, err),
    };
    if children.is_empty() {
        return p.error(&archive, "nothing to extract");
    }
    let mut resolver = Resolver::default();
    let placed = match mode {
        ExtractMode::Open => {
            // Keep it where it is: the staging folder becomes the opened copy.
            let opened = dest.join(children[0].file_name().unwrap_or_default());
            if let Err(err) = fs::rename(&children[0], &opened) {
                return p.error(&opened, err);
            }
            p.set_outcome(match open_with {
                Some(target) => Outcome::OpenWith(opened, target),
                None => Outcome::Open(opened),
            });
            return;
        }
        ExtractMode::Smart if children.len() == 1 => {
            let child = &children[0];
            place(child, &dest.join(name(child)), &mut resolver, p)
        }
        ExtractMode::Smart => {
            let folder = unique_path(&dest, OsStr::new(&stem(&archive)));
            move_staging(&staging.0, &folder, p)
        }
        ExtractMode::Subfolder => {
            let folder = dest.join(stem(&archive));
            if fs::symlink_metadata(&folder).is_err() {
                move_staging(&staging.0, &folder, p)
            } else {
                for child in &children {
                    place(child, &folder.join(name(child)), &mut resolver, p);
                }
                Some(folder)
            }
        }
        ExtractMode::Into => {
            let mut placed: Vec<_> = children
                .iter()
                .filter_map(|child| place(child, &dest.join(name(child)), &mut resolver, p))
                .collect();
            placed.sort();
            placed.into_iter().next()
        }
    };
    if p.cancelled() {
        return;
    }
    if let Some(placed) = placed {
        p.set_outcome(Outcome::Reveal(placed));
    }
    if trash_after
        && !p.has_errors()
        && let Err(err) = trash::delete(&archive)
    {
        p.error(&archive, format!("couldn't move to trash: {err}"));
    }
}

fn name(path: &Path) -> &OsStr {
    path.file_name().unwrap_or_default()
}

fn move_staging(staging: &Path, folder: &Path, p: &Progress) -> Option<PathBuf> {
    match fs::rename(staging, folder) {
        Ok(()) => Some(folder.to_path_buf()),
        Err(err) => {
            p.error(folder, err);
            None
        }
    }
}

/// Moves `src` to `dst`, merging folders and asking about files that exist.
/// Returns where it ended up, or `None` if it was skipped or failed.
fn place(src: &Path, dst: &Path, resolver: &mut Resolver, p: &Progress) -> Option<PathBuf> {
    if p.cancelled() {
        return None;
    }
    let Ok(existing) = fs::symlink_metadata(dst) else {
        return rename(src, dst, p).then(|| dst.to_path_buf());
    };
    let src_is_dir = fs::symlink_metadata(src).is_ok_and(|m| m.is_dir());
    if src_is_dir && existing.is_dir() {
        match fs::read_dir(src) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    place(&entry.path(), &dst.join(entry.file_name()), resolver, p);
                }
            }
            Err(err) => p.error(src, err),
        }
        return Some(dst.to_path_buf());
    }
    match resolver.choose(src, dst, p)? {
        ConflictChoice::Skip => None,
        ConflictChoice::Overwrite => match jobs::remove_all(dst) {
            Ok(()) => rename(src, dst, p).then(|| dst.to_path_buf()),
            Err(err) => {
                p.error(dst, err);
                None
            }
        },
        ConflictChoice::KeepBoth => {
            let other = unique_path(dst.parent()?, name(dst));
            rename(src, &other, p).then_some(other)
        }
    }
}

/// Renames, or copies when the destination is on another drive (a mount
/// point inside the destination).
fn rename(src: &Path, dst: &Path, p: &Progress) -> bool {
    match fs::rename(src, dst) {
        Ok(()) => true,
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
            let mut buffer = vec![0; 1 << 20];
            jobs::copy_tree(src, dst, p, &mut buffer)
        }
        Err(err) => {
            p.error(dst, err);
            false
        }
    }
}

/// Deletes files opened from archives a while ago.
fn clean_opened() {
    let Ok(entries) = fs::read_dir(open_dir()) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).unwrap_or_default() > KEEP_OPENED);
        if old {
            let _ = jobs::remove_all(&entry.path());
        }
    }
}
