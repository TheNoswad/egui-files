//! MD5 and SHA-256 of a file, on a background thread.

use std::{
    fs::File,
    io::Read,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use md5::{Digest, Md5};
use sha2::Sha256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sums {
    pub md5: String,
    pub sha256: String,
}

#[derive(Default)]
struct State {
    done: AtomicU64,
    cancel: AtomicBool,
    result: Mutex<Option<Result<Sums, String>>>,
}

/// Stops when dropped.
pub struct Hashing {
    state: Arc<State>,
    pub total: u64,
}

impl Hashing {
    pub fn start(ctx: &egui::Context, path: PathBuf, total: u64) -> Self {
        let state = Arc::new(State::default());
        let (s, ctx) = (state.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("properties-hash".into())
            .spawn(move || {
                let result = hash(&s, &path, &ctx);
                if !s.cancel.load(Relaxed) {
                    *s.result.lock().unwrap() = Some(result);
                    ctx.request_repaint();
                }
            })
            .expect("spawn thread");
        Self { state, total }
    }

    pub fn done_bytes(&self) -> u64 {
        self.state.done.load(Relaxed)
    }

    pub fn result(&self) -> Option<Result<Sums, String>> {
        self.state.result.lock().unwrap().clone()
    }
}

impl Drop for Hashing {
    fn drop(&mut self) {
        self.state.cancel.store(true, Relaxed);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hash(state: &State, path: &PathBuf, ctx: &egui::Context) -> Result<Sums, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let (mut md5, mut sha) = (Md5::new(), Sha256::new());
    let mut buf = vec![0; 1 << 20];
    let mut last_repaint = Instant::now();
    loop {
        if state.cancel.load(Relaxed) {
            return Err("Cancelled".to_owned());
        }
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        md5.update(&buf[..n]);
        sha.update(&buf[..n]);
        state.done.fetch_add(n as u64, Relaxed);
        if last_repaint.elapsed() > Duration::from_millis(100) {
            last_repaint = Instant::now();
            ctx.request_repaint();
        }
    }
    Ok(Sums {
        md5: hex(&md5.finalize()),
        sha256: hex(&sha.finalize()),
    })
}

/// Which sum `text` (pasted by the user) matches, if any.
pub fn matches(sums: &Sums, text: &str) -> Option<&'static str> {
    let text: String = text.trim().to_lowercase();
    if text.is_empty() {
        None
    } else if text == sums.md5 {
        Some("MD5")
    } else if text == sums.sha256 {
        Some("SHA-256")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_a_file() {
        let path = std::env::temp_dir().join(format!("egui-files-hash-{}", std::process::id()));
        std::fs::write(&path, "abc").unwrap();
        let state = State::default();
        let sums = hash(&state, &path, &egui::Context::default()).unwrap();
        assert_eq!(sums.md5, "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            sums.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            matches(&sums, " 900150983CD24FB0D6963F7D28E17F72\n"),
            Some("MD5")
        );
        assert_eq!(matches(&sums, "nope"), None);
        std::fs::remove_file(path).unwrap();
    }
}
