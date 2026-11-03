// Trunk's initializer: shows the download's progress, then gets out of the way.
export default function () {
    const loading = document.getElementById("loading");
    const bar = loading.querySelector("progress");
    const detail = loading.querySelector(".detail");
    const mb = (bytes) => (bytes / 1e6).toFixed(1);
    return {
        onProgress({ current, total }) {
            if (total > 0) {
                bar.value = current / total;
                detail.textContent = `${mb(current)} of ${mb(total)} MB`;
            }
        },
        onSuccess() {
            loading.remove();
        },
        onFailure(error) {
            bar.remove();
            loading.firstElementChild.textContent = "The demo couldn't start.";
            detail.textContent = String(error);
        },
    };
}
