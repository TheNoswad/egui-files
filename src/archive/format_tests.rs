use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use super::*;
use crate::jobs::{Answer, ConflictChoice, Outcome, Progress, Prompt};

fn scratch(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("egui-files-archive-{name}-{}", std::process::id()));
    let _ = disk::remove_dir_all(&root);
    disk::create_dir_all(&root).unwrap();
    root
}

/// `src/project/{readme.md, run.sh (executable), empty, docs/guide.txt, link -> readme.md}`
fn project(root: &Path) -> PathBuf {
    let dir = root.join("src/project");
    disk::create_dir_all(dir.join("docs")).unwrap();
    disk::create_dir_all(dir.join("empty-dir")).unwrap();
    disk::write(dir.join("readme.md"), "# hello\n".repeat(100)).unwrap();
    disk::write(dir.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    disk::write(dir.join("empty"), "").unwrap();
    disk::write(dir.join("docs/guide.txt"), "x".repeat(300_000)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        disk::set_permissions(dir.join("run.sh"), disk::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("readme.md", dir.join("link")).unwrap();
    }
    dir
}

/// Path → contents (`<dir>`, `-> target`, or the text), plus executable bits.
fn snapshot(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for entry in disk::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let name = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let meta = disk::symlink_metadata(&path).unwrap();
            if meta.is_symlink() {
                out.insert(
                    name,
                    format!("-> {}", disk::read_link(&path).unwrap().display()),
                );
            } else if meta.is_dir() {
                out.insert(name, "<dir>".into());
                walk(base, &path, out);
            } else {
                #[cfg(unix)]
                let exec =
                    std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o100 != 0;
                #[cfg(not(unix))]
                let exec = false;
                let text = disk::read_to_string(&path).unwrap();
                out.insert(
                    name,
                    format!("{}{}", if exec { "x " } else { "" }, text.len()),
                );
            }
        }
    }
    walk(dir, dir, &mut out);
    out
}

fn compress_to(
    sources: Vec<PathBuf>,
    dest: PathBuf,
    format: Format,
    password: Option<&str>,
) -> PathBuf {
    let p = Progress::default();
    compress(
        CompressJob {
            sources,
            dest,
            format,
            level: 3,
            password: password.map(str::to_owned),
            encrypt_names: true,
        },
        &p,
    );
    assert_eq!(p.errors(), Vec::<String>::new());
    match p.take_outcome() {
        Some(Outcome::Reveal(path)) => path,
        other => panic!("unexpected outcome {other:?}"),
    }
}

fn extract_with(
    job: ExtractJob,
    answer: impl Fn(&Prompt) -> Answer + Send + 'static,
    prompts: usize,
) -> (Option<Outcome>, Vec<String>) {
    let p = std::sync::Arc::new(Progress::default());
    let worker = {
        let p = p.clone();
        std::thread::spawn(move || extract(job, &p))
    };
    for _ in 0..prompts {
        p.answer(&answer);
    }
    worker.join().unwrap();
    (p.take_outcome(), p.errors())
}

fn extract_quietly(job: ExtractJob) -> Option<Outcome> {
    let (outcome, errors) = extract_with(job, |_| unreachable!("no prompts expected"), 0);
    assert_eq!(errors, Vec::<String>::new());
    outcome
}

#[test]
fn every_format_round_trips() {
    let root = scratch("round-trip");
    let project = project(&root);
    let expected = snapshot(&project);
    for format in Format::CREATE.iter().copied() {
        let out = root.join(format!("out-{format:?}"));
        disk::create_dir_all(&out).unwrap();
        let archive = compress_to(
            vec![project.clone()],
            out.join(format!("project{}", format.extension())),
            format,
            None,
        );
        assert_eq!(archive, out.join(format!("project{}", format.extension())));

        let listing = read::list(&archive, None).unwrap();
        assert_eq!(listing.format, format);
        let files: Vec<_> = listing
            .entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.path.as_str())
            .collect();
        assert!(
            files.contains(&"project/docs/guide.txt"),
            "{format:?}: {files:?}"
        );
        // Symlinks count their target's length in some formats.
        assert!(
            (300_818..300_830).contains(&listing.total_size()),
            "{format:?}"
        );

        // One top-level folder: extracted as is, next to the archive.
        let outcome = extract_quietly(ExtractJob::new(
            archive.clone(),
            out.clone(),
            ExtractMode::Smart,
            false,
        ));
        assert_eq!(
            outcome,
            Some(Outcome::Reveal(out.join("project"))),
            "{format:?}"
        );
        assert_eq!(snapshot(&out.join("project")), expected, "{format:?}");
        // Nothing left behind but the archive and the folder.
        let mut names: Vec<_> = disk::read_dir(&out)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2, "{format:?}: {names:?}");
    }
    disk::remove_dir_all(root).unwrap();
}

#[test]
fn several_items_get_a_folder_and_selections_extract_alone() {
    let root = scratch("smart");
    let project = project(&root);
    let sources = vec![project.join("readme.md"), project.join("docs")];
    let archive = compress_to(sources, root.join("bits.zip"), Format::Zip, None);
    let dest = root.join("dest");

    let outcome = extract_quietly(ExtractJob::new(
        archive.clone(),
        dest.clone(),
        ExtractMode::Smart,
        false,
    ));
    assert_eq!(outcome, Some(Outcome::Reveal(dest.join("bits"))));
    assert!(dest.join("bits/readme.md").is_file());
    assert!(dest.join("bits/docs/guide.txt").is_file());

    // Again: a new folder rather than mixing into the first.
    let outcome = extract_quietly(ExtractJob::new(
        archive.clone(),
        dest.clone(),
        ExtractMode::Smart,
        false,
    ));
    assert_eq!(outcome, Some(Outcome::Reveal(dest.join("bits (1)"))));

    // Only `docs/guide.txt`, without its folder.
    let mut job = ExtractJob::new(archive.clone(), dest.clone(), ExtractMode::Into, false);
    job.selection = Some(Selection::new(vec!["docs/guide.txt".into()]));
    assert_eq!(
        extract_quietly(job),
        Some(Outcome::Reveal(dest.join("guide.txt")))
    );
    assert_eq!(
        disk::read_to_string(dest.join("guide.txt")).unwrap().len(),
        300_000
    );

    // Opening a file extracts it on its own to a temporary folder.
    let mut job = ExtractJob::new(archive, PathBuf::new(), ExtractMode::Open, false);
    job.selection = Some(Selection::new(vec!["readme.md".into()]));
    let Some(Outcome::Open(opened)) = extract_quietly(job) else {
        panic!("expected a file to open");
    };
    assert!(opened.starts_with(open_dir()));
    assert_eq!(disk::read_to_string(&opened).unwrap().len(), 800);
    disk::remove_dir_all(opened.parent().unwrap()).unwrap();
    disk::remove_dir_all(root).unwrap();
}

#[test]
fn conflicts_ask_and_remember_the_answer() {
    let root = scratch("conflicts");
    let project = project(&root);
    let archive = compress_to(
        vec![project.join("docs"), project.join("readme.md")],
        root.join("a.tar.zst"),
        Format::TarZst,
        None,
    );
    let dest = root.join("dest");
    disk::create_dir_all(dest.join("docs")).unwrap();
    disk::write(dest.join("docs/guide.txt"), "old").unwrap();
    disk::write(dest.join("docs/mine.txt"), "mine").unwrap();
    disk::write(dest.join("readme.md"), "old").unwrap();

    // Skip, for all: nothing existing is touched, folders merge.
    let job = ExtractJob::new(archive.clone(), dest.clone(), ExtractMode::Into, false);
    let (_, errors) = extract_with(
        job,
        |prompt| {
            assert!(matches!(prompt, Prompt::Conflict { .. }));
            Answer::Conflict {
                choice: ConflictChoice::Skip,
                all: true,
            }
        },
        1,
    );
    assert!(errors.is_empty());
    assert_eq!(
        disk::read_to_string(dest.join("docs/guide.txt")).unwrap(),
        "old"
    );
    assert_eq!(disk::read_to_string(dest.join("readme.md")).unwrap(), "old");
    assert!(dest.join("docs/mine.txt").exists());

    // Keep both, then overwrite.
    let job = ExtractJob::new(archive.clone(), dest.clone(), ExtractMode::Into, false);
    let (_, errors) = extract_with(
        job,
        |prompt| {
            let Prompt::Conflict { existing, .. } = prompt else {
                panic!("expected a conflict")
            };
            let choice = if existing.ends_with("guide.txt") {
                ConflictChoice::KeepBoth
            } else {
                ConflictChoice::Overwrite
            };
            Answer::Conflict { choice, all: false }
        },
        2,
    );
    assert!(errors.is_empty());
    assert_eq!(
        disk::read_to_string(dest.join("docs/guide (1).txt"))
            .unwrap()
            .len(),
        300_000
    );
    assert_eq!(
        disk::read_to_string(dest.join("readme.md")).unwrap().len(),
        800
    );
    // No staging folders left.
    assert!(
        disk::read_dir(&dest)
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().contains("extracting"))
    );
    disk::remove_dir_all(root).unwrap();
}

#[test]
fn passwords_are_asked_for_and_checked() {
    let root = scratch("passwords");
    let project = project(&root);
    for format in [Format::Zip, Format::SevenZ] {
        let archive = compress_to(
            vec![project.clone()],
            root.join(format!("secret{}", format.extension())),
            format,
            Some("hunter2"),
        );
        // 7z hides the names too.
        match (format, read::list(&archive, None)) {
            (Format::SevenZ, Err(Error::NeedsPassword)) => {}
            (Format::Zip, Ok(listing)) => assert!(listing.encrypted()),
            (_, other) => panic!("{format:?}: {:?}", other.map(|l| l.entries.len())),
        }
        assert!(matches!(
            read::list(&archive, Some("nope")),
            Err(Error::WrongPassword) | Ok(_)
        ));

        let dest = root.join(format!("dest-{format:?}"));
        let job = ExtractJob::new(archive.clone(), dest.clone(), ExtractMode::Smart, false);
        let tries = std::sync::atomic::AtomicUsize::new(0);
        let (outcome, errors) = extract_with(
            job,
            move |prompt| {
                let Prompt::Password { wrong, .. } = prompt else {
                    panic!("expected a password prompt")
                };
                let first = tries.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0;
                assert_eq!(*wrong, !first);
                Answer::Password(if first { "wrong" } else { "hunter2" }.into())
            },
            2,
        );
        assert_eq!(errors, Vec::<String>::new(), "{format:?}");
        assert_eq!(outcome, Some(Outcome::Reveal(dest.join("project"))));
        assert_eq!(
            snapshot(&dest.join("project")),
            snapshot(&project),
            "{format:?}"
        );
    }
    disk::remove_dir_all(root).unwrap();
}

#[test]
fn trash_after_extract_only_on_success() {
    let root = scratch("trash-after");
    let project = project(&root);
    let archive = compress_to(vec![project], root.join("p.tar.gz"), Format::TarGz, None);
    // Cancelled jobs keep the archive.
    let p = Progress::default();
    p.cancel_for_test();
    extract(
        ExtractJob::new(archive.clone(), root.join("d"), ExtractMode::Smart, true),
        &p,
    );
    assert!(archive.exists());
    assert!(!root.join("d/project").exists());
    disk::remove_dir_all(root).unwrap();
}

#[test]
fn entries_cannot_escape_the_destination() {
    let root = scratch("escape");
    let archive = root.join("evil.tar");
    {
        let mut tar = tar::Builder::new(disk::File::create(&archive).unwrap());
        let mut add = |name: &[u8], kind: tar::EntryType, link: Option<&str>, data: &[u8]| {
            let mut header = tar::Header::new_gnu();
            // Bypass the tar crate's own checks, like a hostile archive would.
            header.as_old_mut().name[..name.len()].copy_from_slice(name);
            header.set_entry_type(kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            if let Some(link) = link {
                header.set_link_name(link).unwrap();
            }
            header.set_cksum();
            tar.append(&header, data).unwrap();
        };
        add(b"../escaped.txt", tar::EntryType::Regular, None, b"bad");
        add(b"ok/inside.txt", tar::EntryType::Regular, None, b"fine");
        // A link out, then a file written through it.
        add(
            b"ok/out",
            tar::EntryType::Symlink,
            Some(root.to_str().unwrap()),
            b"",
        );
        add(
            b"ok/out/through-link.txt",
            tar::EntryType::Regular,
            None,
            b"bad",
        );
        tar.finish().unwrap();
    }
    let dest = root.join("dest");
    let (_, errors) = extract_with(
        ExtractJob::new(archive, dest.clone(), ExtractMode::Into, false),
        |_| unreachable!(),
        0,
    );
    assert!(!root.join("escaped.txt").exists());
    assert!(!root.join("through-link.txt").exists());
    assert_eq!(
        disk::read_to_string(dest.join("ok/inside.txt")).unwrap(),
        "fine"
    );
    // The escaping name and the link that collides with a folder were reported.
    assert_eq!(errors.len(), 2, "{errors:?}");
    disk::remove_dir_all(root).unwrap();
}

/// Our archives open in other tools, and theirs open here.
#[test]
fn compatible_with_system_tools() {
    let have = |tool: &str| Command::new(tool).arg("--help").output().is_ok();
    let root = scratch("compat");
    let project = project(&root);
    let expected = snapshot(&project);
    let check = |tool: &str, args: &[&str], dest: &Path| {
        disk::create_dir_all(dest).unwrap();
        let output = Command::new(tool)
            .args(args)
            .current_dir(dest)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{tool} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(snapshot(&dest.join("project")), expected, "{tool} {args:?}");
    };
    if have("bsdtar") {
        for format in [Format::TarGz, Format::TarXz, Format::TarZst, Format::Zip] {
            let archive = compress_to(
                vec![project.clone()],
                root.join(format!("ours{}", format.extension())),
                format,
                None,
            );
            check(
                "bsdtar",
                &["-xf", archive.to_str().unwrap()],
                &root.join(format!("bsdtar-{format:?}")),
            );
        }
    }
    if have("7z") {
        let archive = compress_to(
            vec![project.clone()],
            root.join("ours.7z"),
            Format::SevenZ,
            Some("pw"),
        );
        check(
            "7z",
            &["x", "-ppw", archive.to_str().unwrap()],
            &root.join("7z-7z"),
        );
        let archive = compress_to(
            vec![project.clone()],
            root.join("ours-aes.zip"),
            Format::Zip,
            Some("pw"),
        );
        check(
            "7z",
            &["x", "-ppw", archive.to_str().unwrap()],
            &root.join("7z-zip"),
        );

        // 7-Zip's own archives, encrypted names included.
        let theirs = root.join("theirs.7z");
        let status = Command::new("7z")
            .args([
                "a",
                "-snl",
                "-mhe=on",
                "-psecret",
                theirs.to_str().unwrap(),
                "project",
            ])
            .current_dir(project.parent().unwrap())
            .output()
            .unwrap();
        assert!(status.status.success());
        let mut job = ExtractJob::new(theirs, root.join("from-7z"), ExtractMode::Smart, false);
        job.password = Some("secret".into());
        extract_quietly(job);
        assert_eq!(snapshot(&root.join("from-7z/project")), expected);
    }
    if have("zip") {
        let theirs = root.join("theirs.zip");
        let status = Command::new("zip")
            .args(["-qry", "-P", "old", theirs.to_str().unwrap(), "project"])
            .current_dir(project.parent().unwrap())
            .output()
            .unwrap();
        assert!(status.status.success());
        // ZipCrypto, the classic zip encryption.
        let job = ExtractJob::new(theirs, root.join("from-zip"), ExtractMode::Smart, false);
        let (_, errors) = extract_with(job, |_| Answer::Password("old".into()), 1);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(snapshot(&root.join("from-zip/project")), expected);
    }
    disk::remove_dir_all(root).unwrap();
}

/// The rar samples that ship with the unrar crate, if it's in the cargo registry.
fn rar_samples() -> Option<PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))?;
    disk::read_dir(home.join("registry/src"))
        .ok()?
        .flatten()
        .flat_map(|index| disk::read_dir(index.path()).into_iter().flatten().flatten())
        .map(|krate| krate.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("unrar-"))
        })
        .map(|krate| krate.join("data"))
        .filter(|data| data.is_dir())
}

#[test]
fn extracts_rar() {
    let Some(samples) = rar_samples() else {
        eprintln!("no rar samples found; skipping");
        return;
    };
    let root = scratch("rar");
    let listing = read::list(&samples.join("crypted.rar"), None).unwrap();
    assert!(listing.encrypted());
    let job = ExtractJob::new(
        samples.join("crypted.rar"),
        root.clone(),
        ExtractMode::Into,
        false,
    );
    let (outcome, errors) = extract_with(job, |_| Answer::Password("unrar".into()), 1);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(outcome, Some(Outcome::Reveal(root.join(".gitignore"))));
    assert_eq!(disk::read(root.join(".gitignore")).unwrap().len(), 18);

    // Encrypted headers: no listing without the password.
    let hidden = samples.join("comment-hpw-password.rar");
    assert!(matches!(
        read::list(&hidden, None),
        Err(Error::NeedsPassword)
    ));
    assert!(read::list(&hidden, Some("password")).is_ok());
    disk::remove_dir_all(root).unwrap();
}
