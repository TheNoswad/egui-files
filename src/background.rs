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

/// Lets the browser get on with things for `ms` milliseconds, or as soon as it
/// can with 0: its only thread is also the page's.
#[cfg(target_arch = "wasm32")]
pub async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// Lets the page draw and take input, if work has had the thread for most of
/// a frame.
#[cfg(target_arch = "wasm32")]
pub async fn pause() {
    use std::cell::Cell;
    use web_time::{Duration, Instant};
    const WORK_PER_FRAME: Duration = Duration::from_millis(12);
    thread_local! {
        static RESUMED: Cell<Option<Instant>> = const { Cell::new(None) };
    }
    if RESUMED
        .get()
        .is_none_or(|at| at.elapsed() >= WORK_PER_FRAME)
    {
        sleep(0).await;
        RESUMED.set(Some(Instant::now()));
    }
}
