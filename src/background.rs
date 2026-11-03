//! One-off work kept off the UI thread.

/// Runs `work` on a thread of its own. The browser can't start threads, so
/// there it runs on the UI thread once the current frame is done: later, not
/// right away, so it never runs while its caller holds a lock it takes.
pub fn spawn(name: &str, work: impl FnOnce() + Send + 'static) {
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(work)
        .unwrap_or_else(|err| panic!("couldn't start a thread for {name}: {err}"));
    #[cfg(target_arch = "wasm32")]
    {
        let _ = name;
        wasm_bindgen_futures::spawn_local(async move { work() });
    }
}
