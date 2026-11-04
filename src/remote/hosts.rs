//! Computers to suggest connecting to: the hosts named in `~/.ssh/config`
//! (and the files it includes), and those in `~/.ssh/known_hosts` that
//! aren't hashed. Read again every so often, as they're small.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use web_time::Instant;

/// How long what was read is used before reading it again.
const FRESH: Duration = Duration::from_secs(10);
/// How deep `Include`s go, as ssh allows.
const MAX_DEPTH: usize = 16;

/// A host named in the ssh config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    /// As the config names it, which is what to connect to.
    pub alias: String,
    /// Where it leads: `user@hostname:port`, from what the config says,
    /// if it says anything.
    pub detail: Option<String>,
}

#[derive(Clone, Default)]
pub struct Hosts {
    pub config: Vec<Host>,
    /// From known_hosts, as `host` or `host:port`, without those in config.
    pub known: Vec<String>,
}

impl Hosts {
    /// Every name, config's first, matching `prefix` (ignoring case).
    pub fn matching(&self, prefix: &str, recent: &[String]) -> Vec<String> {
        let prefix = prefix.to_lowercase();
        let mut names: Vec<String> = Vec::new();
        let all = recent
            .iter()
            .chain(self.config.iter().map(|h| &h.alias))
            .chain(&self.known);
        for name in all {
            let fits = name.to_lowercase().starts_with(&prefix)
                // `user@host`: the host part counts too.
                || name
                    .rsplit_once('@')
                    .is_some_and(|(_, host)| host.to_lowercase().starts_with(&prefix));
            if fits && !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    }
}

static CACHE: Mutex<Option<(Instant, Hosts)>> = Mutex::new(None);

/// The hosts to suggest, read afresh if a while has passed.
pub fn hosts() -> Hosts {
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((at, hosts)) = &*cache
        && at.elapsed() < FRESH
    {
        return hosts.clone();
    }
    let hosts = read_hosts();
    *cache = Some((Instant::now(), hosts.clone()));
    hosts
}

fn ssh_dir() -> Option<PathBuf> {
    #[cfg(target_arch = "wasm32")]
    return None;
    #[cfg(not(target_arch = "wasm32"))]
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".ssh"))
}

fn read_hosts() -> Hosts {
    let Some(dir) = ssh_dir() else {
        return Hosts::default();
    };
    let mut config = Vec::new();
    read_config(&dir, &dir.join("config"), 0, &mut config);
    let known = disk::read_to_string(dir.join("known_hosts"))
        .map(|text| parse_known_hosts(&text))
        .unwrap_or_default()
        .into_iter()
        .filter(|name| !config.iter().any(|h: &Host| &h.alias == name))
        .collect();
    Hosts { config, known }
}

fn read_config(dir: &Path, file: &Path, depth: usize, hosts: &mut Vec<Host>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(text) = disk::read_to_string(file) else {
        return;
    };
    parse_config(&text, hosts, &mut |pattern, hosts| {
        for included in expand_include(dir, pattern) {
            read_config(dir, &included, depth + 1, hosts);
        }
    });
}

/// A config line's keyword (lowercase) and arguments, which are split at
/// spaces or a `=`, and may be quoted.
fn split_line(line: &str) -> Option<(String, Vec<String>)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (keyword, rest) = line
        .split_once(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or((line, ""));
    let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
    let mut args = Vec::new();
    let mut chars = rest.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut arg = String::new();
        if c == '"' {
            chars.next();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                arg.push(c);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                arg.push(c);
                chars.next();
            }
        }
        if arg.starts_with('#') {
            break;
        }
        args.push(arg);
    }
    Some((keyword.to_ascii_lowercase(), args))
}

/// Adds the hosts `text` names to `hosts`; `include` gets each `Include`'s
/// patterns, where they come.
fn parse_config(text: &str, hosts: &mut Vec<Host>, include: &mut dyn FnMut(&str, &mut Vec<Host>)) {
    // The hosts the current `Host` block names, by index into `hosts`.
    let mut block: Vec<usize> = Vec::new();
    let mut details: Vec<(Option<String>, Option<String>, Option<String>)> = Vec::new();
    let finish = |hosts: &mut Vec<Host>,
                  block: &[usize],
                  details: &[(Option<String>, Option<String>, Option<String>)]| {
        for (&i, (user, name, port)) in block.iter().zip(details) {
            if user.is_none() && name.is_none() && port.is_none() {
                continue;
            }
            let mut detail = name.clone().unwrap_or_else(|| hosts[i].alias.clone());
            if let Some(user) = user {
                detail = format!("{user}@{detail}");
            }
            if let Some(port) = port {
                detail = format!("{detail}:{port}");
            }
            hosts[i].detail = Some(detail);
        }
    };
    for line in text.lines() {
        let Some((keyword, args)) = split_line(line) else {
            continue;
        };
        match keyword.as_str() {
            "host" | "match" => {
                finish(hosts, &block, &details);
                block.clear();
                details.clear();
                if keyword == "match" {
                    continue;
                }
                // Patterns aren't hosts; nor is anything a `!` rules out.
                for name in args {
                    let pattern = name.contains(['*', '?', '!']);
                    if pattern || hosts.iter().any(|h| h.alias == name) {
                        continue;
                    }
                    block.push(hosts.len());
                    details.push((None, None, None));
                    hosts.push(Host {
                        alias: name,
                        detail: None,
                    });
                }
            }
            "include" => args.iter().for_each(|pattern| include(pattern, hosts)),
            // The first value given counts, as in ssh.
            "user" | "hostname" | "port" => {
                for detail in &mut details {
                    let slot = match keyword.as_str() {
                        "user" => &mut detail.0,
                        "hostname" => &mut detail.1,
                        _ => &mut detail.2,
                    };
                    if slot.is_none() {
                        *slot = args.first().cloned();
                    }
                }
            }
            _ => {}
        }
    }
    finish(hosts, &block, &details);
}

/// The files an `Include` names: relative to `~/.ssh`, with `~` for home,
/// and `*` and `?` in the last part.
fn expand_include(dir: &Path, pattern: &str) -> Vec<PathBuf> {
    let path = match pattern.strip_prefix("~/") {
        Some(rest) => dir
            .parent()
            .map_or_else(|| dir.join(rest), |home| home.join(rest)),
        None => dir.join(pattern),
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !name.contains(['*', '?']) {
        return vec![path];
    }
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = disk::read_dir(parent)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| wildcard(&name, &e.file_name().to_string_lossy()))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    // ssh reads them in order.
    found.sort();
    found
}

/// Whether `text` matches `pattern`, where `*` is any run of characters and
/// `?` any one.
fn wildcard(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// The hosts known_hosts names in plain text: `host`, or `host:port` for
/// `[host]:port`. Hashed ones can't be read, by design.
fn parse_known_hosts(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let mut hosts = fields.next().unwrap_or_default();
        // @cert-authority and @revoked come first; neither is a host.
        if hosts.starts_with('@') {
            continue;
        }
        if hosts.starts_with('|') {
            continue;
        }
        hosts = hosts.trim();
        for host in hosts.split(',') {
            if host.starts_with('!') || host.contains(['*', '?']) {
                continue;
            }
            let name = match host.strip_prefix('[').and_then(|h| h.split_once("]:")) {
                Some((host, "22")) => host.to_owned(),
                Some((host, port)) if host.contains(':') => format!("[{host}]:{port}"),
                Some((host, port)) => format!("{host}:{port}"),
                None => host.to_owned(),
            };
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (Vec<Host>, Vec<String>) {
        let mut hosts = Vec::new();
        let mut included = Vec::new();
        parse_config(text, &mut hosts, &mut |p, _| included.push(p.to_owned()));
        (hosts, included)
    }

    #[test]
    fn config_hosts_and_where_they_lead() {
        let (hosts, included) = parse(
            "# mine\n\
             Include config.d/*\n\
             Host pi nas\n\
             \x20 HostName 192.168.1.5\n\
             \x20 User = me\n\
             \x20 User other\n\
             Host *.example.com !secret.example.com\n\
             \x20 User work\n\
             Host=box\n\
             \x20 Port 2200\n\
             Host plain\n\
             Match host foo\n\
             \x20 User nobody\n\
             Host \"quoted name\" pi\n",
        );
        assert_eq!(included, ["config.d/*"]);
        let detail = |alias: &str| {
            hosts
                .iter()
                .find(|h| h.alias == alias)
                .unwrap()
                .detail
                .clone()
        };
        let aliases: Vec<_> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(aliases, ["pi", "nas", "box", "plain", "quoted name"]);
        assert_eq!(detail("pi").as_deref(), Some("me@192.168.1.5"));
        assert_eq!(detail("nas").as_deref(), Some("me@192.168.1.5"));
        assert_eq!(detail("box").as_deref(), Some("box:2200"));
        assert_eq!(detail("plain"), None);
    }

    #[test]
    fn known_hosts_in_plain_text() {
        let text = "github.com,140.82.121.4 ssh-ed25519 AAAA\n\
                    [127.0.0.1]:2222 ssh-ed25519 AAAA\n\
                    [example.org]:22 ssh-rsa AAAA\n\
                    [::1]:2200 ssh-rsa AAAA\n\
                    |1|hashed=|salt= ssh-ed25519 AAAA\n\
                    @cert-authority *.corp ssh-rsa AAAA\n\
                    github.com ssh-rsa AAAA\n";
        assert_eq!(
            parse_known_hosts(text),
            [
                "github.com",
                "140.82.121.4",
                "127.0.0.1:2222",
                "example.org",
                "[::1]:2200"
            ]
        );
    }

    #[test]
    fn wildcards() {
        assert!(wildcard("*", "anything"));
        assert!(wildcard("*.conf", "work.conf"));
        assert!(!wildcard("*.conf", "work.conf~"));
        assert!(wildcard("h?st*", "host-a"));
        assert!(!wildcard("a*b", "acd"));
    }

    #[test]
    fn suggestions_match_hosts_and_users() {
        let hosts = Hosts {
            config: vec![Host {
                alias: "pi".into(),
                detail: None,
            }],
            known: vec!["github.com".into(), "pinto.lan".into()],
        };
        let recent = ["me@pebble".to_owned(), "pi".to_owned()];
        assert_eq!(
            hosts.matching("p", &recent),
            ["me@pebble", "pi", "pinto.lan"]
        );
        assert_eq!(hosts.matching("GIT", &recent), ["github.com"]);
    }
}
