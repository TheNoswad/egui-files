//! Retrying changes that failed with "permission denied" as root or an
//! administrator: the app runs a copy of itself with [`FLAG`] through
//! pkexec (Linux) or UAC (Windows), which applies the plan and exits
//! without opening a window.

use std::{
    io::{self, Read, Write},
    sync::{Arc, Mutex},
};

use super::edit::{self, Failure, Plan, Report};

/// The argument that makes the app apply a plan instead of starting.
pub const FLAG: &str = "--apply-properties";

/// If the app was started as the helper, does its work and returns the
/// exit code.
pub fn run_helper() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next()? != FLAG {
        return None;
    }
    Some(match helper(args.collect()) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    })
}

fn helper(args: Vec<std::ffi::OsString>) -> io::Result<()> {
    let to_io = |e: toml::de::Error| io::Error::new(io::ErrorKind::InvalidData, e.to_string());
    // Linux: the plan on stdin and the report on stdout. Windows can't pipe
    // into an elevated process, so they go through files named on the
    // command line.
    let text = match args.first() {
        Some(plan) => std::fs::read_to_string(plan)?,
        None => {
            let mut text = String::new();
            io::stdin().read_to_string(&mut text)?;
            text
        }
    };
    let plan: Plan = toml::from_str(&text).map_err(to_io)?;
    let failures = edit::apply(&plan, &edit::Progress::default());
    let report = toml::to_string(&Report { failures }).map_err(io::Error::other)?;
    match args.get(1) {
        Some(out) => std::fs::write(out, report),
        None => io::stdout().write_all(report.as_bytes()),
    }
}

/// The verb for the button, per platform.
pub fn label() -> &'static str {
    if cfg!(windows) {
        "Retry as administrator"
    } else {
        "Retry as root"
    }
}

/// Whether this platform can retry elevated.
pub fn available() -> bool {
    cfg!(any(target_os = "linux", windows))
}

/// What's still failing, or why elevating didn't work.
type Outcome = Result<Vec<Failure>, String>;

/// Runs a plan elevated on a background thread.
pub struct Running {
    result: Arc<Mutex<Option<Outcome>>>,
}

impl Running {
    pub fn start(ctx: &egui::Context, plan: Plan) -> Self {
        let result = Arc::new(Mutex::new(None));
        let (r, ctx) = (result.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("properties-elevated".into())
            .spawn(move || {
                let outcome = run(&plan).map_err(|e| e.to_string());
                *r.lock().unwrap() = Some(outcome);
                ctx.request_repaint();
            })
            .expect("spawn thread");
        Self { result }
    }

    /// What's still failing, or why elevating didn't work, once done.
    pub fn take(&self) -> Option<Outcome> {
        self.result.lock().unwrap().take()
    }
}

fn parse_report(text: &str) -> io::Result<Vec<Failure>> {
    toml::from_str::<Report>(text)
        .map(|r| r.failures)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

#[cfg(target_os = "linux")]
fn run(plan: &Plan) -> io::Result<Vec<Failure>> {
    use std::process::{Command, Stdio};
    let text = toml::to_string(plan).map_err(io::Error::other)?;
    let exe = std::env::current_exe()?;
    let mut child = Command::new("pkexec")
        .arg(exe)
        .arg(FLAG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => {
                io::Error::new(e.kind(), "pkexec isn't installed (it comes with polkit)")
            }
            _ => e,
        })?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(text.as_bytes())?;
    let output = child.wait_with_output()?;
    match output.status.code() {
        Some(0) => parse_report(&String::from_utf8_lossy(&output.stdout)),
        // pkexec: the dialog was dismissed, or authorization was refused.
        Some(126) => Err(io::Error::other("Authentication was cancelled")),
        Some(127) => Err(io::Error::other("Not authorized")),
        _ => Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        )),
    }
}

#[cfg(windows)]
fn run(plan: &Plan) -> io::Result<Vec<Failure>> {
    let text = toml::to_string(plan).map_err(io::Error::other)?;
    let stem = temp_stem("properties");
    let plan_file = std::env::temp_dir().join(format!("{stem}.toml"));
    let report_file = std::env::temp_dir().join(format!("{stem}-report.toml"));
    std::fs::write(&plan_file, text)?;
    let _cleanup = Cleanup(vec![plan_file.clone(), report_file.clone()]);
    run_as_admin(&format!(
        "{FLAG} \"{}\" \"{}\"",
        plan_file.display(),
        report_file.display()
    ))?;
    parse_report(&std::fs::read_to_string(&report_file)?)
}

/// A file name stem in the temp folder that no other helper run uses.
#[cfg(windows)]
pub fn temp_stem(what: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("egui-files-{what}-{}-{n}", std::process::id())
}

/// Runs this app again as an administrator (the UAC prompt) with `params`,
/// waits for it, and fails unless it exits with 0.
#[cfg(windows)]
pub fn run_as_admin(params: &str) -> io::Result<()> {
    use windows::{
        Win32::{
            Foundation::{CloseHandle, ERROR_CANCELLED},
            System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject},
            UI::{
                Shell::{
                    SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
                },
                WindowsAndMessaging::SW_HIDE,
            },
        },
        core::{HSTRING, PCWSTR},
    };
    let exe = HSTRING::from(std::env::current_exe()?.as_os_str());
    let params = HSTRING::from(params);
    let verb = HSTRING::from("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: `info` and the strings it points to outlive the call.
    if let Err(error) = unsafe { ShellExecuteExW(&mut info) } {
        return Err(if error.code() == ERROR_CANCELLED.to_hresult() {
            io::Error::other("Elevation was cancelled")
        } else {
            io::Error::other(error)
        });
    }
    let mut code = 1u32;
    // SAFETY: `hProcess` is the helper's handle, which we own.
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
    }
    if code != 0 {
        return Err(io::Error::other(format!(
            "The helper failed (exit code {code})"
        )));
    }
    Ok(())
}

#[cfg(windows)]
pub struct Cleanup(pub Vec<std::path::PathBuf>);

#[cfg(windows)]
impl Drop for Cleanup {
    fn drop(&mut self) {
        for file in &self.0 {
            let _ = std::fs::remove_file(file);
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn run(_plan: &Plan) -> io::Result<Vec<Failure>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "unsupported here",
    ))
}
