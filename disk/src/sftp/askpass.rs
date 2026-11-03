//! Passwords, passphrases and host keys: ssh asks about them through the
//! program `SSH_ASKPASS` names, which is the app itself started again
//! ([`main`]). That copy hands the question to the running app over a socket
//! on 127.0.0.1, which asks whoever [`super::set_prompter`] named, and
//! passes the answer back to ssh.
//!
//! Each connection gets a token of its own, so other programs can't put
//! questions to the user in its name or take the answers.

use std::{
    collections::HashMap,
    hash::{BuildHasher, RandomState},
    io::{self, BufRead, BufReader, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::Command,
    sync::{Mutex, OnceLock, PoisonError},
};

use super::{Prompt, PromptKind};

const ADDRESS_VAR: &str = "EGUI_FILES_ASKPASS";
const TOKEN_VAR: &str = "EGUI_FILES_ASKPASS_TOKEN";

struct Server {
    addr: SocketAddr,
    /// Each live connection's token, and the host it's for.
    tokens: Mutex<HashMap<String, String>>,
}

static SERVER: OnceLock<io::Result<Server>> = OnceLock::new();

fn server() -> io::Result<&'static Server> {
    SERVER
        .get_or_init(|| {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let addr = listener.local_addr()?;
            std::thread::Builder::new()
                .name("askpass".into())
                .spawn(move || {
                    for stream in listener.incoming().flatten() {
                        let _ = std::thread::Builder::new()
                            .name("askpass question".into())
                            .spawn(move || {
                                let _ = answer(stream);
                            });
                    }
                })?;
            Ok(Server {
                addr,
                tokens: Mutex::default(),
            })
        })
        .as_ref()
        .map_err(|err| io::Error::new(err.kind(), err.to_string()))
}

fn random_token() -> String {
    let a = RandomState::new().hash_one(std::time::SystemTime::now());
    let b = RandomState::new().hash_one(super::super::process_id());
    format!("{a:016x}{b:016x}")
}

/// Questions from `command`'s ssh, asked through `program`, come to the prompter, as about `host`,
/// until the returned guard is dropped.
pub fn set_up(command: &mut Command, host: &str, program: &Path) -> io::Result<Registration> {
    let server = server()?;
    let token = random_token();
    server
        .tokens
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(token.clone(), host.to_owned());
    command
        .env("SSH_ASKPASS", program)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env(ADDRESS_VAR, server.addr.to_string())
        .env(TOKEN_VAR, &token);
    // ssh before 8.4 asks only with a display to show the question on.
    #[cfg(unix)]
    if std::env::var_os("DISPLAY").is_none() {
        command.env("DISPLAY", ":0");
    }
    Ok(Registration(token))
}

/// Keeps a connection's token valid; see [`set_up`].
pub struct Registration(String);

impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(server) = server() {
            server
                .tokens
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&self.0);
        }
    }
}

/// Puts one question from an askpass to the prompter.
fn answer(stream: TcpStream) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut token = String::new();
    reader.read_line(&mut token)?;
    let mut kind = String::new();
    reader.read_line(&mut kind)?;
    let mut text = String::new();
    reader.take(64 * 1024).read_to_string(&mut text)?;
    let host = server()?
        .tokens
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(token.trim())
        .cloned();
    let Some(host) = host else {
        return Ok(());
    };
    let kind = prompt_kind(kind.trim(), &text);
    let reply = super::prompt(Prompt {
        host,
        text: text.trim().to_owned(),
        kind,
    });
    let mut stream = stream;
    match reply {
        Some(answer) => {
            stream.write_all(b"1")?;
            stream.write_all(answer.as_bytes())?;
        }
        None => stream.write_all(b"0")?,
    }
    stream.shutdown(Shutdown::Write)
}

/// What sort of question ssh asks: `SSH_ASKPASS_PROMPT` says, for some.
fn prompt_kind(asked_as: &str, text: &str) -> PromptKind {
    match asked_as {
        "confirm" => PromptKind::Confirm,
        "none" => PromptKind::Notice,
        _ if text.contains("(yes/no") => PromptKind::HostKey,
        _ => PromptKind::Secret,
    }
}

/// When this process was started as ssh's askpass, asks the running app the
/// question in its arguments, prints the answer for ssh and returns the
/// exit code. Otherwise `None`, at once.
pub fn main() -> Option<i32> {
    let addr = std::env::var(ADDRESS_VAR).ok()?;
    let token = std::env::var(TOKEN_VAR).ok()?;
    let text = std::env::args().nth(1).unwrap_or_default();
    let kind = std::env::var("SSH_ASKPASS_PROMPT").unwrap_or_default();
    let ask = || -> io::Result<Option<String>> {
        let mut stream = TcpStream::connect(addr.as_str())?;
        // A newline would end the line early; ssh's questions have none
        // in them where they matter, but be safe.
        let kind = kind.replace('\n', " ");
        stream.write_all(format!("{token}\n{kind}\n{text}").as_bytes())?;
        stream.shutdown(Shutdown::Write)?;
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply)?;
        Ok(match reply.split_first() {
            Some((b'1', answer)) => Some(String::from_utf8_lossy(answer).into_owned()),
            _ => None,
        })
    };
    Some(match ask() {
        Ok(Some(answer)) => {
            let mut out = io::stdout();
            let _ = writeln!(out, "{answer}");
            let _ = out.flush();
            0
        }
        _ => 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_questions_apart() {
        let host_key = "The authenticity of host 'h (1.2.3.4)' can't be established.\nED25519 key fingerprint is SHA256:x.\nAre you sure you want to continue connecting (yes/no/[fingerprint])?";
        assert_eq!(prompt_kind("", host_key), PromptKind::HostKey);
        assert_eq!(prompt_kind("", "user@h's password: "), PromptKind::Secret);
        assert_eq!(prompt_kind("confirm", "Allow use of key?"), PromptKind::Confirm);
        assert_eq!(prompt_kind("none", "Confirm user presence"), PromptKind::Notice);
    }

    #[test]
    fn tokens_are_unguessable_enough() {
        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }
}
