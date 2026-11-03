//! One SFTP connection: `ssh -s <host> sftp` as a child process, speaking
//! SFTP over its pipes. Any number of threads can make requests at once; a
//! thread of its own reads the replies and hands each to whoever asked.

use std::{
    collections::HashMap,
    io::{self, BufReader, Read, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU32, Ordering::Relaxed},
        mpsc::{self, Receiver, SyncSender},
    },
};

use super::{
    Target, askpass,
    wire::{self, Reader, Reply, Writer},
};

/// Replies are rarely more than one read's worth of data; a bigger length
/// means the stream is out of step.
const MAX_PACKET: usize = 4 << 20;

type Waiter = SyncSender<io::Result<Reply>>;

#[derive(Default)]
struct State {
    waiting: HashMap<u32, Waiter>,
    /// Why the connection ended, once it has.
    closed: Option<String>,
}

struct Shared {
    state: Mutex<State>,
    /// The end of what ssh said on stderr, for when it fails.
    stderr: Mutex<String>,
}

impl Shared {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stderr(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Ends the connection for everyone waiting on it, and anyone who asks
    /// from now on.
    fn close(&self, why: String) {
        let mut state = self.state();
        state.closed.get_or_insert(why);
        // Dropping the senders wakes each waiter with an error.
        state.waiting.clear();
    }
}

/// A request sent, its reply still to come.
pub struct Pending {
    reply: Receiver<io::Result<Reply>>,
    shared: Arc<Shared>,
}

impl Pending {
    pub fn wait(self) -> io::Result<Reply> {
        match self.reply.recv() {
            Ok(reply) => reply,
            Err(_) => Err(lost(&self.shared)),
        }
    }
}

fn lost(shared: &Shared) -> io::Error {
    let why = shared
        .state()
        .closed
        .clone()
        .unwrap_or_else(|| "the connection closed".into());
    io::Error::new(io::ErrorKind::ConnectionAborted, why)
}

pub struct Session {
    shared: Arc<Shared>,
    input: Mutex<ChildStdin>,
    child: Arc<Mutex<Child>>,
    next_id: AtomicU32,
    extensions: HashMap<String, Vec<u8>>,
    /// The most a read or write may carry.
    pub max_read: u32,
    pub max_write: u32,
    /// Where `.` is: the user's home folder, normally.
    pub home: String,
}

/// What ssh is told, beyond the user's own configuration: no forwarding,
/// nothing run locally, and notice a connection that's gone quiet.
const SSH_OPTIONS: &[&str] = &[
    "ClearAllForwardings=yes",
    "ForwardAgent=no",
    "ForwardX11=no",
    "PermitLocalCommand=no",
    "ServerAliveInterval=15",
    "ServerAliveCountMax=3",
    "ConnectTimeout=20",
];

/// The command that connects to `target` and starts its SFTP server.
pub fn ssh_command(target: &Target) -> Command {
    let mut command = Command::new(super::ssh_program());
    command.arg("-T");
    for option in SSH_OPTIONS {
        command.arg("-o").arg(option);
    }
    if let Some(port) = target.port {
        command.arg("-p").arg(port.to_string());
    }
    if let Some(user) = &target.user {
        command.arg("-l").arg(user);
    }
    // `--` so a host can't pass for an option; Target refuses those anyway.
    command.args(["-s", "--", &target.host, "sftp"]);
    command
}

impl Session {
    /// Starts `command` (see [`ssh_command`]) and sets up SFTP with it. Any
    /// password or host key ssh asks about goes through its askpass, which
    /// `command` sets up.
    pub fn start(mut command: Command, asking: &askpass::Registration) -> io::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: no console flashing up for ssh.
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn().map_err(|err| {
            if err.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "couldn't find ssh: is OpenSSH installed?",
                )
            } else {
                err
            }
        })?;
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            stderr: Mutex::default(),
        });
        let mut stderr = child.stderr.take().expect("piped");
        let mut input = child.stdin.take().expect("piped");
        let mut output = BufReader::new(child.stdout.take().expect("piped"));
        let child = Arc::new(Mutex::new(child));
        asking.watch(child.clone());
        let errors = shared.clone();
        std::thread::Builder::new()
            .name("ssh stderr".into())
            .spawn(move || {
                let mut buf = [0; 1024];
                while let Ok(n @ 1..) = stderr.read(&mut buf) {
                    let mut text = errors.stderr.lock().unwrap_or_else(PoisonError::into_inner);
                    text.push_str(&String::from_utf8_lossy(&buf[..n]));
                    // Keep the end, where the reason is.
                    if text.len() > 4096 {
                        let cut = text.len() - 2048;
                        let cut = (cut..text.len())
                            .find(|&i| text.is_char_boundary(i))
                            .unwrap_or(0);
                        text.drain(..cut);
                    }
                }
            })?;

        let version = Writer::default().u8(wire::INIT).u32(3).0;
        let handshake = write_packet(&mut input, &version)
            .and_then(|()| read_packet(&mut output))
            .and_then(|packet| parse_version(&packet));
        let extensions = match handshake {
            Ok(extensions) => extensions,
            Err(err) => {
                // ssh exits once it gives up; its reason is on stderr.
                let _ = child.lock().unwrap_or_else(PoisonError::into_inner).wait();
                if asking.cancelled() {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                let reason = failure_reason(&shared.stderr());
                return Err(if reason.is_empty() {
                    err
                } else {
                    io::Error::new(io::ErrorKind::ConnectionRefused, reason)
                });
            }
        };

        let reader = shared.clone();
        std::thread::Builder::new()
            .name("sftp replies".into())
            .spawn(move || read_replies(output, &reader))?;
        let mut session = Self {
            shared,
            input: Mutex::new(input),
            child,
            next_id: AtomicU32::new(1),
            extensions,
            max_read: 32 * 1024,
            max_write: 32 * 1024,
            home: String::new(),
        };
        if session.has_extension("limits@openssh.com") {
            let reply = session
                .extended("limits@openssh.com", Writer::default())?
                .wait()?;
            if let Reply::Extended(body) = reply {
                let mut r = Reader(&body);
                let _max_packet = r.u64()?;
                let clamp = |n: u64| u32::try_from(n.clamp(4096, 1 << 20)).unwrap();
                let (read, write) = (r.u64()?, r.u64()?);
                // Zero means no limit given.
                if read > 0 {
                    session.max_read = clamp(read);
                }
                if write > 0 {
                    session.max_write = clamp(write);
                }
            }
        }
        session.home = session
            .request(wire::REALPATH, Writer::default().str("."))?
            .wait()?
            .name()?;
        Ok(session)
    }

    pub fn has_extension(&self, name: &str) -> bool {
        self.extensions.contains_key(name)
    }

    /// Whether it's still connected, as far as anyone knows.
    pub fn is_open(&self) -> bool {
        self.shared.state().closed.is_none()
    }

    /// Sends a request; its reply comes from [`Pending::wait`].
    pub fn request(&self, kind: u8, body: Writer) -> io::Result<Pending> {
        let id = self.next_id.fetch_add(1, Relaxed);
        let (send, reply) = mpsc::sync_channel(1);
        {
            let mut state = self.shared.state();
            if state.closed.is_some() {
                drop(state);
                return Err(lost(&self.shared));
            }
            state.waiting.insert(id, send);
        }
        let mut packet = Writer::default().u8(kind).u32(id).0;
        packet.extend_from_slice(&body.0);
        let sent = write_packet(
            &mut *self.input.lock().unwrap_or_else(PoisonError::into_inner),
            &packet,
        );
        if let Err(err) = sent {
            self.shared.close(format!("couldn't send to ssh: {err}"));
        }
        Ok(Pending {
            reply,
            shared: self.shared.clone(),
        })
    }

    pub fn extended(&self, name: &str, body: Writer) -> io::Result<Pending> {
        let mut request = Writer::default().str(name);
        request.0.extend_from_slice(&body.0);
        self.request(wire::EXTENDED, request)
    }

    /// Sends a request and waits for its reply.
    pub fn call(&self, kind: u8, body: Writer) -> io::Result<Reply> {
        self.request(kind, body)?.wait()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shared.close("disconnected".into());
        let mut child = self.child.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn write_packet(to: &mut impl Write, body: &[u8]) -> io::Result<()> {
    let len = u32::try_from(body.len()).expect("requests are small");
    let mut packet = Vec::with_capacity(4 + body.len());
    packet.extend_from_slice(&len.to_be_bytes());
    packet.extend_from_slice(body);
    to.write_all(&packet)?;
    to.flush()
}

fn read_packet(from: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0; 4];
    from.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_PACKET {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the server's reply makes no sense (is something printing from the remote shell's startup files?)",
        ));
    }
    let mut packet = vec![0; len];
    from.read_exact(&mut packet)?;
    Ok(packet)
}

fn parse_version(packet: &[u8]) -> io::Result<HashMap<String, Vec<u8>>> {
    let mut r = Reader(packet);
    if r.u8()? != wire::VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the server didn't start SFTP",
        ));
    }
    let version = r.u32()?;
    if version < 3 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("the server only speaks SFTP version {version}"),
        ));
    }
    let mut extensions = HashMap::new();
    while !r.0.is_empty() {
        let name = r.string()?;
        extensions.insert(name, r.bytes()?);
    }
    Ok(extensions)
}

fn read_replies(mut output: BufReader<ChildStdout>, shared: &Shared) {
    let why = loop {
        let packet = match read_packet(&mut output) {
            Ok(packet) => packet,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                let reason = failure_reason(&shared.stderr());
                break if reason.is_empty() {
                    "the connection closed".into()
                } else {
                    reason
                };
            }
            Err(err) => break err.to_string(),
        };
        let mut r = Reader(&packet);
        let (Ok(kind), Ok(id)) = (r.u8(), r.u32()) else {
            break "the server's reply makes no sense".into();
        };
        let reply = Reply::parse(kind, r.0);
        if let Some(waiter) = shared.state().waiting.remove(&id) {
            // They may have stopped waiting (a read-ahead no longer wanted).
            let _ = waiter.try_send(reply);
        }
    };
    shared.close(why);
}

/// The useful part of what ssh said as it failed: its last lines, without
/// the chatter about adding host keys.
pub fn failure_reason(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("Warning: Permanently added"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_round_trip() {
        let mut buf = Vec::new();
        write_packet(&mut buf, &[1, 2, 3]).unwrap();
        assert_eq!(buf, [0, 0, 0, 3, 1, 2, 3]);
        assert_eq!(read_packet(&mut &buf[..]).unwrap(), [1, 2, 3]);
        // A shell's greeting where a packet should be.
        assert!(read_packet(&mut &b"Welcome to the server"[..]).is_err());
    }

    #[test]
    fn failure_reasons_skip_host_key_notes() {
        let stderr = "Warning: Permanently added 'h' (ED25519) to the list of known hosts.\r\nuser@h: Permission denied (publickey).\r\n";
        assert_eq!(
            failure_reason(stderr),
            "user@h: Permission denied (publickey)."
        );
    }
}
