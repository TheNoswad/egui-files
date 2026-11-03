// Trunk's initializer: shows the download's progress, then gets out of the
// way. In the landing page's frame it tells the page too, which shows the
// progress over its screenshot and brings the app in once it's drawn.
export default function () {
    const loading = document.getElementById("loading");
    const bar = loading.querySelector("progress");
    const detail = loading.querySelector(".detail");
    const mb = (bytes) => (bytes / 1e6).toFixed(1);
    const fail = (error) => {
        loading.querySelector("progress")?.remove();
        loading.firstElementChild.textContent = "The demo couldn't start.";
        detail.textContent = error;
        tell({ failed: error });
    };
    const tell = (message) => {
        if (window.parent !== window) {
            window.parent.postMessage({ eguiFilesDemo: message }, "*");
        }
    };
    // From the app (src/lib.rs), once it's running; its first frame follows.
    addEventListener("egui-files-started", () => {
        loading.remove();
        requestAnimationFrame(() => requestAnimationFrame(() => tell({ ready: true })));
    });
    addEventListener("egui-files-failed", () =>
        fail("Your browser may lack WebGPU and WebGL; the console has more."));
    return {
        onProgress({ current, total }) {
            if (total > 0) {
                bar.value = current / total;
                detail.textContent = `${mb(current)} of ${mb(total)} MB`;
                tell({ progress: current / total });
            }
        },
        onSuccess() {
            detail.textContent = "Starting…";
        },
        onFailure(error) {
            fail(String(error));
        },
    };
}
