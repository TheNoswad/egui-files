//! Against a real server: set `EGUI_FILES_SFTP_TEST_CONFIG` to an ssh config
//! naming these hosts, all one server with a user whose password is
//! `secret` and who lets in `key`. Without it, these tests pass without
//! doing anything.
//!
//! ```text
//! Host sftp-test sftp-test-not-yet
//!   HostName 127.0.0.1
//!   Port 2222
//!   User tester
//!   IdentityFile /path/to/key
//!   IdentitiesOnly yes
//!   UserKnownHostsFile /path/to/known_hosts
//! # Only by password (cancelled, for the second).
//! Host sftp-test-password sftp-test-cancel
//!   HostName 127.0.0.1
//!   Port 2222
//!   User tester
//!   PubkeyAuthentication no
//!   UserKnownHostsFile /path/to/known_hosts
//! # Its own known hosts, which start out empty.
//! Host sftp-test-new-key
//!   HostName 127.0.0.1
//!   Port 2222
//!   User tester
//!   IdentityFile /path/to/key
//!   IdentitiesOnly yes
//!   UserKnownHostsFile /path/to/empty_known_hosts
//!   StrictHostKeyChecking ask
//! # Let in by nothing.
//! Host sftp-test-nobody
//!   HostName 127.0.0.1
//!   Port 2222
//!   User tester
//!   IdentityFile /nonexistent
//!   IdentitiesOnly yes
//!   PasswordAuthentication no
//!   KbdInteractiveAuthentication no
//!   UserKnownHostsFile /path/to/known_hosts
//! ```
//!
//! An OpenSSH server in a container does (Alpine's `openssh-server`, with
//! `PasswordAuthentication yes`). The askpass is the `askpass` example,
//! which `cargo test` builds.

use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{Mutex, Once},
};

use super::*;
use crate::{self as disk};

static PROMPTS: Mutex<Vec<Prompt>> = Mutex::new(Vec::new());

/// The server to test against, or `None` to skip.
fn server(host: &str) -> Option<PathBuf> {
    let config = std::env::var_os("EGUI_FILES_SFTP_TEST_CONFIG")?;
    static SET_UP: Once = Once::new();
    SET_UP.call_once(|| {
        // Each alias on a connection of its own: they're all the one user
        // on the one server, which would share one otherwise.
        set_extra_ssh_args([
            OsString::from("-F"),
            config,
            "-o".into(),
            "ControlPath=none".into(),
        ]);
        let askpass = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join("../examples/askpass");
        set_askpass_program(askpass);
        set_prompter(|prompt| {
            let answer = match prompt.kind {
                // The user thinks better of it.
                PromptKind::Secret if prompt.host == "sftp-test-cancel" => None,
                PromptKind::Secret => Some("secret".to_owned()),
                PromptKind::HostKey => Some("yes".to_owned()),
                _ => None,
            };
            PROMPTS.lock().unwrap().push(prompt);
            answer
        });
    });
    Some(url(host, "/"))
}

/// A folder of the test's own in the home folder on `sftp-test`.
fn scratch(name: &str) -> Option<PathBuf> {
    let root = server("sftp-test")?;
    connect(&root).unwrap();
    let dir = home(&root).unwrap().join(format!("disk-tests-{name}"));
    let _ = disk::remove_dir_all(&dir);
    disk::create_dir_all(&dir).unwrap();
    Some(dir)
}

#[test]
fn paths() {
    let location = parse(Path::new("sftp://me@host:2222/a/./b/../c/")).unwrap();
    assert_eq!(location.authority, "me@host:2222");
    assert_eq!(location.path, "/a/c");
    assert_eq!(location.to_path(), PathBuf::from("sftp://me@host:2222/a/c"));
    assert_eq!(location.parent().unwrap().path, "/a");
    let top = parse(Path::new("sftp://host")).unwrap();
    assert_eq!(top.path, "/");
    assert_eq!(top.parent(), None);
    assert_eq!(top.to_path(), PathBuf::from("sftp://host"));
    assert_eq!(top.join("x").path, "/x");
    // Joined on Windows.
    assert_eq!(parse(Path::new("sftp://host/a\\b")).unwrap().path, "/a/b");
    assert_eq!(
        Target::parse("u@[::1]:22"),
        Some(Target {
            user: Some("u".into()),
            host: "::1".into(),
            port: Some(22),
        })
    );
    // Nothing ssh would take for an option.
    assert!(parse(Path::new("sftp://-oProxyCommand=x/")).is_none());
    assert!(Target::parse("-l@host").is_none());
    assert!(Target::parse("a b").is_none());
    assert!(Target::parse("host:port").is_none());
    assert!(!is_remote(Path::new("/home/sftp://x")));
    assert_eq!(
        root(Path::new("sftp://h/a/b")),
        Some(PathBuf::from("sftp://h"))
    );
}

#[test]
fn files_and_folders() {
    let Some(dir) = scratch("files") else { return };
    let file = dir.join("a.txt");
    disk::write(&file, "hello").unwrap();
    assert_eq!(disk::read_to_string(&file).unwrap(), "hello");
    let meta = disk::metadata(&file).unwrap();
    assert!(meta.is_file() && !meta.is_dir());
    assert_eq!(meta.len(), 5);
    assert!(meta.modified().is_ok());
    assert!(disk::is_dir(&dir));
    assert!(disk::exists(&file).unwrap());
    assert!(!disk::exists(dir.join("missing")).unwrap());
    assert_eq!(
        disk::metadata(dir.join("missing")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );

    // Names taken.
    assert_eq!(
        disk::File::create_new(&file).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(
        disk::create_dir(&dir).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );

    disk::create_dir_all(dir.join("x/y/z")).unwrap();
    disk::write(dir.join("x/y/z/deep"), "deep").unwrap();
    assert_eq!(
        disk::remove_dir(dir.join("x")).unwrap_err().kind(),
        io::ErrorKind::DirectoryNotEmpty
    );
    let mut names: Vec<_> = disk::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["a.txt", "x"]);

    disk::rename(&file, dir.join("b.txt")).unwrap();
    assert!(!disk::exists(&file).unwrap());
    // Over a file that's there, as on Unix.
    disk::write(dir.join("c.txt"), "c").unwrap();
    disk::rename(dir.join("b.txt"), dir.join("c.txt")).unwrap();
    assert_eq!(disk::read_to_string(dir.join("c.txt")).unwrap(), "hello");
    assert_eq!(
        disk::rename(dir.join("c.txt"), "/tmp/anywhere")
            .unwrap_err()
            .kind(),
        io::ErrorKind::CrossesDevices
    );

    disk::symlink("c.txt", dir.join("link")).unwrap();
    assert_eq!(
        disk::read_link(dir.join("link")).unwrap(),
        Path::new("c.txt")
    );
    assert!(
        disk::symlink_metadata(dir.join("link"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(disk::metadata(dir.join("link")).unwrap().len(), 5);
    assert_eq!(
        disk::canonicalize(dir.join("link")).unwrap(),
        dir.join("c.txt")
    );

    disk::remove_dir_all(dir.join("x")).unwrap();
    assert!(!disk::exists(dir.join("x")).unwrap());
    disk::remove_file(dir.join("link")).unwrap();
    disk::remove_dir_all(&dir).unwrap();
}

#[test]
fn listings_say_what_links_lead_to() {
    let Some(dir) = scratch("listings") else {
        return;
    };
    disk::create_dir(dir.join("sub")).unwrap();
    disk::symlink("sub", dir.join("to-sub")).unwrap();
    disk::symlink("nowhere", dir.join("broken")).unwrap();
    forget(&dir);
    let entries: Vec<_> = disk::read_dir(&dir).unwrap().map(Result::unwrap).collect();
    let find = |name: &str| entries.iter().find(|e| e.file_name() == name).unwrap();
    assert!(find("to-sub").file_type().unwrap().is_symlink());
    assert!(is_listed(&dir));
    // From what the listing said, with the server none the wiser.
    assert!(disk::metadata(dir.join("to-sub")).unwrap().is_dir());
    assert!(disk::metadata(dir.join("broken")).is_err());
    assert!(
        disk::symlink_metadata(dir.join("broken"))
            .unwrap()
            .is_symlink()
    );
    disk::remove_dir_all(&dir).unwrap();
}

#[test]
fn big_files_seeking_and_appending() {
    let Some(dir) = scratch("big") else { return };
    let file = dir.join("big.bin");
    // Several reads' worth, and not a multiple of one.
    let data: Vec<u8> = (0..3_000_017u32).map(|i| (i * 7 % 251) as u8).collect();
    disk::write(&file, &data).unwrap();
    assert_eq!(disk::metadata(&file).unwrap().len(), data.len() as u64);
    assert_eq!(disk::read(&file).unwrap(), data);

    let mut f = disk::File::open(&file).unwrap();
    let mut buf = [0; 10];
    f.seek(SeekFrom::Start(1_000_000)).unwrap();
    f.read_exact(&mut buf).unwrap();
    assert_eq!(buf, data[1_000_000..1_000_010]);
    f.seek(SeekFrom::Current(-5)).unwrap();
    f.read_exact(&mut buf).unwrap();
    assert_eq!(buf, data[1_000_005..1_000_015]);
    assert_eq!(f.seek(SeekFrom::End(-3)).unwrap(), data.len() as u64 - 3);
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, data[data.len() - 3..]);
    drop(f);

    let mut f = disk::OpenOptions::new().append(true).open(&file).unwrap();
    f.write_all(b"end").unwrap();
    drop(f);
    assert_eq!(disk::metadata(&file).unwrap().len(), data.len() as u64 + 3);

    let f = disk::OpenOptions::new().write(true).open(&file).unwrap();
    f.set_len(4).unwrap();
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    f.set_modified(when).unwrap();
    drop(f);
    let meta = disk::metadata(&file).unwrap();
    assert_eq!((meta.len(), meta.modified().unwrap()), (4, when));
    disk::remove_dir_all(&dir).unwrap();
}

#[test]
fn copies_between_here_and_there() {
    let Some(dir) = scratch("copies") else { return };
    let local = std::env::temp_dir().join(format!("disk-sftp-copies-{}", std::process::id()));
    std::fs::write(&local, "local").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o640)).unwrap();
    }
    assert_eq!(disk::copy(&local, dir.join("up")).unwrap(), 5);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = disk::metadata(dir.join("up")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
    }
    let down = local.with_extension("down");
    disk::copy(dir.join("up"), &down).unwrap();
    assert_eq!(std::fs::read_to_string(&down).unwrap(), "local");
    disk::copy(dir.join("up"), dir.join("again")).unwrap();
    // On the server itself.
    let from = disk::File::open(dir.join("up")).unwrap();
    let to = disk::File::create(dir.join("there")).unwrap();
    assert!(from.copy_on_server(&to, 0, 5).unwrap());
    drop((from, to));
    assert_eq!(disk::read_to_string(dir.join("there")).unwrap(), "local");
    assert_eq!(disk::read_to_string(dir.join("again")).unwrap(), "local");
    std::fs::remove_file(local).unwrap();
    std::fs::remove_file(down).unwrap();
    disk::remove_dir_all(&dir).unwrap();
}

#[test]
fn others_changes_are_found_by_listing_again() {
    let Some(dir) = scratch("relist") else { return };
    disk::write(dir.join("a"), "a").unwrap();
    disk::read_dir(&dir).unwrap().for_each(drop);
    assert!(!relist(&dir).unwrap());
    // As someone else would, behind the cache's back.
    let session = session("sftp-test").unwrap();
    let path = parse(&dir.join("b")).unwrap().path;
    let open = Writer::default()
        .str(&path)
        .u32(wire::OPEN_WRITE | wire::OPEN_CREATE)
        .attrs(&Attrs::default());
    let handle = session.call(wire::OPEN, open).unwrap().handle().unwrap();
    session
        .call(wire::CLOSE, Writer::default().bytes(&handle))
        .unwrap()
        .ok()
        .unwrap();
    take_changes();
    assert!(relist(&dir).unwrap());
    assert!(take_changes().contains(&dir));
    assert!(!relist(&dir).unwrap());
    disk::remove_dir_all(&dir).unwrap();
}

#[test]
fn changes_are_noted() {
    let Some(dir) = scratch("changes") else {
        return;
    };
    take_changes();
    disk::write(dir.join("new"), "x").unwrap();
    assert!(take_changes().contains(&dir.join("new")));
    disk::remove_dir_all(&dir).unwrap();
    assert!(take_changes().contains(&dir));
}

#[test]
fn passwords_and_new_host_keys_are_asked_about() {
    let Some(root) = server("sftp-test-new-key") else {
        return;
    };
    connect(&root).unwrap();
    assert_eq!(status(&root), Status::Connected);
    let password = server("sftp-test-password").unwrap();
    connect(&password).unwrap();
    assert!(disk::is_dir(home(&password).unwrap()));
    let prompts = PROMPTS.lock().unwrap();
    assert!(
        prompts
            .iter()
            .any(|p| p.kind == PromptKind::HostKey && p.host == "sftp-test-new-key"),
        "{prompts:?}"
    );
    assert!(
        prompts
            .iter()
            .any(|p| p.kind == PromptKind::Secret && p.host == "sftp-test-password"),
        "{prompts:?}"
    );
    drop(prompts);
    disconnect(&password);
    assert_eq!(status(&password), Status::Disconnected);
}

#[test]
fn cancelling_a_password_stops_connecting() {
    let Some(root) = server("sftp-test-cancel") else {
        return;
    };
    let err = connect(&root).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Interrupted, "{err}");
    let asked = PROMPTS
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.host == "sftp-test-cancel")
        .count();
    // Not asked again with an empty answer.
    assert_eq!(asked, 1);
}

#[test]
fn the_ui_thread_never_waits_to_connect() {
    let Some(root) = server("sftp-test-not-yet") else {
        return;
    };
    std::thread::spawn(move || {
        never_connect_on_this_thread();
        let err = disk::metadata(&root).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
        assert_eq!(status(&root), Status::Disconnected);
    })
    .join()
    .unwrap();
}

#[test]
fn failures_say_why() {
    let Some(root) = server("sftp-test-nobody") else {
        return;
    };
    let err = connect(&root).unwrap_err();
    assert!(err.to_string().contains("Permission denied"), "{err}");
    assert!(matches!(status(&root), Status::Failed(_)));
    // It stays failed for what doesn't try again.
    let err = disk::metadata(&root).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotConnected);
}

/// A folder and server for testing without a network: "ssh" here just runs
/// the machine's SFTP server, refusing a second channel to `nochannel`. `None`
/// without one, or when the tests are set up for a real server.
#[cfg(unix)]
fn local_server(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    if std::env::var_os("EGUI_FILES_SFTP_TEST_CONFIG").is_some() {
        return None;
    }
    let program = ["/usr/lib/ssh/sftp-server", "/usr/lib/openssh/sftp-server"]
        .into_iter()
        .find(|path| Path::new(path).exists())?;
    let dir = std::env::temp_dir().join(format!("egui-files-sftp-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    static SET_UP: Once = Once::new();
    SET_UP.call_once(|| {
        let script = std::env::temp_dir().join(format!(
            "egui-files-fake-ssh-{}",
            std::process::id()
        ));
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$*\" in *BatchMode=yes*nochannel*) exit 1;; esac\nexec {program}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        set_ssh_program(&script);
        set_askpass_program("/bin/false");
    });
    Some(dir)
}

#[cfg(unix)]
fn location_in(host: &str, file: &Path) -> Location {
    parse(&url(host, file.to_str().unwrap())).unwrap()
}

#[cfg(unix)]
fn test_data(len: u32) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 251) as u8).collect()
}

#[cfg(unix)]
#[test]
fn looking_at_a_files_start_keeps_what_was_read() {
    let Some(dir) = local_server("look") else {
        return;
    };
    let path = dir.join("big.bin");
    let data = test_data(3_000_017);
    std::fs::write(&path, &data).unwrap();
    let mut file = open(
        &location_in("local", &path),
        Open {
            read: true,
            ..Open::default()
        },
    )
    .unwrap();
    let mut start = [0; 16];
    file.read_exact(&mut start).unwrap();
    assert_eq!(file.seek(SeekFrom::Start(0)).unwrap(), 0);
    let mut again = [0; 1000];
    file.read_exact(&mut again).unwrap();
    assert_eq!(again, data[..1000]);
    assert_eq!(file.reads_asked, 1, "the first read was enough");
    // And it all still lines up, reading on from there.
    let mut rest = Vec::new();
    file.read_to_end(&mut rest).unwrap();
    assert_eq!(rest, data[1000..]);
    drop(file);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[test]
fn seeking_back_after_a_write_doesnt_read_what_was_there() {
    let Some(dir) = local_server("rewrite") else {
        return;
    };
    let path = dir.join("file.bin");
    std::fs::write(&path, test_data(1000)).unwrap();
    let mut file = open(
        &location_in("local", &path),
        Open {
            read: true,
            write: true,
            ..Open::default()
        },
    )
    .unwrap();
    let mut first = [0; 10];
    file.read_exact(&mut first).unwrap();
    file.write_all(b"new").unwrap();
    file.seek(SeekFrom::Start(10)).unwrap();
    let mut read = [0; 3];
    file.read_exact(&mut read).unwrap();
    assert_eq!(&read, b"new");
    drop(file);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[test]
fn reading_all_of_a_listed_file_needs_one_round_of_requests() {
    let Some(dir) = local_server("whole") else {
        return;
    };
    let path = dir.join("big.bin");
    let data = test_data(1_500_001);
    std::fs::write(&path, &data).unwrap();
    let location = location_in("local", &path);
    // Its size comes from here, as from a listing.
    assert_eq!(stat(&location, true).unwrap().size, Some(data.len() as u64));
    let mut file = open(
        &location,
        Open {
            read: true,
            ..Open::default()
        },
    )
    .unwrap();
    let mut read = Vec::new();
    file.read_to_end(&mut read).unwrap();
    assert_eq!(read, data);
    let session_max = u64::from(file.session_max_read());
    let needed = (data.len() as u64).div_ceil(session_max) as u32;
    assert_eq!(file.reads_asked, needed, "and nothing to find the end");
    drop(file);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[test]
fn files_have_a_channel_of_their_own_and_copy_on_it() {
    let Some(dir) = local_server("channel") else {
        return;
    };
    let (from, to) = (dir.join("from"), dir.join("to"));
    std::fs::write(&from, b"data").unwrap();
    let mut source = open(
        &location_in("local", &from),
        Open {
            read: true,
            ..Open::default()
        },
    )
    .unwrap();
    let mut target = open(
        &location_in("local", &to),
        Open {
            write: true,
            create: true,
            ..Open::default()
        },
    )
    .unwrap();
    let main = session("local").unwrap();
    assert!(source.on_session(&target) && !source.is_on(&main));
    assert!(source.copy_on_server(&mut target, 0, 4).unwrap());
    drop((source, target));
    assert_eq!(std::fs::read(&to).unwrap(), b"data");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[test]
fn files_share_the_connection_without_a_channel() {
    let Some(dir) = local_server("nochannel") else {
        return;
    };
    let path = dir.join("file");
    std::fs::write(&path, b"data").unwrap();
    let mut file = open(
        &location_in("nochannel", &path),
        Open {
            read: true,
            ..Open::default()
        },
    )
    .unwrap();
    assert!(file.is_on(&session("nochannel").unwrap()));
    let mut read = Vec::new();
    file.read_to_end(&mut read).unwrap();
    assert_eq!(read, b"data");
    drop(file);
    std::fs::remove_dir_all(&dir).unwrap();
}
