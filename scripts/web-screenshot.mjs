// Takes a screenshot of a page in headless Firefox, over WebDriver BiDi (which
// Firefox speaks itself: no driver needed), once the web demo has started and
// settled. Used by scripts/screenshots.sh.
//
//   node scripts/web-screenshot.mjs <url> <light|dark> <out.png> <profile-dir>
//
// The page is 1366×682 CSS pixels, the size of the landing page's stage.
// Errors the page logs (a Rust panic among them) are printed, and fail it.

import { spawn } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";

const [url, theme, out, profile] = process.argv.slice(2);
if (!profile || !["light", "dark"].includes(theme)) {
    console.error("usage: web-screenshot.mjs <url> <light|dark> <out.png> <profile-dir>");
    process.exit(2);
}
const settle = Number(process.env.EGUI_FILES_SCREENSHOT_AFTER ?? 6) * 1000;
const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

// The theme the page sees, and no first-run pages.
mkdirSync(profile, { recursive: true });
writeFileSync(
    `${profile}/user.js`,
    [
        ["layout.css.prefers-color-scheme.content-override", theme === "dark" ? 0 : 1],
        ["browser.shell.checkDefaultBrowser", false],
        ["browser.startup.homepage_override.mstone", "ignore"],
        ["datareporting.policy.dataSubmissionEnabled", false],
        ["toolkit.telemetry.reportingpolicy.firstRun", false],
    ]
        .map(([name, value]) => `user_pref(${JSON.stringify(name)}, ${JSON.stringify(value)});`)
        .join("\n") + "\n",
);

const port = 9300 + Math.floor(Math.random() * 600);
const firefox = spawn(
    "firefox",
    ["--headless", "--no-remote", "--profile", profile, `--remote-debugging-port=${port}`],
    // Which setting the viewport's size needs.
    { stdio: "ignore", env: { ...process.env, MOZ_REMOTE_ALLOW_SYSTEM_ACCESS: "1" } },
);
let failed = false;
const stop = (code) => {
    firefox.kill();
    process.exit(code);
};
setTimeout(() => {
    console.error("web-screenshot: timed out");
    stop(1);
}, 120_000);

let socket;
for (let tries = 0; !socket; tries++) {
    try {
        socket = await new Promise((done, fail) => {
            const ws = new WebSocket(`ws://127.0.0.1:${port}/session`);
            ws.onopen = () => done(ws);
            ws.onerror = fail;
        });
    } catch {
        if (tries > 100) {
            console.error("web-screenshot: Firefox didn't start");
            stop(1);
        }
        await sleep(200);
    }
}

let next = 0;
const waiting = new Map();
socket.onmessage = ({ data }) => {
    const message = JSON.parse(data);
    if (message.id !== undefined) {
        const { done, fail } = waiting.get(message.id);
        waiting.delete(message.id);
        if (message.type === "error") fail(new Error(`${message.error}: ${message.message}`));
        else done(message.result);
    } else if (message.method === "log.entryAdded" && message.params.level === "error") {
        console.error(`page: ${message.params.text}`, JSON.stringify(message.params.stackTrace ?? message.params.source));
        failed = true;
    }
};
const send = (method, params = {}) =>
    new Promise((done, fail) => {
        const id = next++;
        waiting.set(id, { done, fail });
        socket.send(JSON.stringify({ id, method, params }));
    });

try {
    await send("session.new", { capabilities: {} });
    await send("session.subscribe", { events: ["log.entryAdded"] });
    const { contexts } = await send("browsingContext.getTree");
    const context = contexts[0].context;
    // web/demo/index.html's signal that the app is running.
    await send("script.addPreloadScript", {
        functionDeclaration:
            "() => addEventListener('egui-files-started', () => { window.eguiFilesStarted = true; })",
    });
    // The viewport can only be sized once the tab has a page of its own: an
    // empty one, so the demo loads once, at its size.
    await send("browsingContext.navigate", { context, url: "data:text/html,", wait: "complete" });
    await send("browsingContext.setViewport", { context, viewport: { width: 1366, height: 682 } });
    await send("browsingContext.navigate", { context, url, wait: "complete" });
    for (;;) {
        const { result } = await send("script.evaluate", {
            expression: "window.eguiFilesStarted === true",
            target: { context },
            awaitPromise: false,
        });
        if (result.value) break;
        await sleep(200);
    }
    await sleep(settle);
    const { data } = await send("browsingContext.captureScreenshot", { context });
    writeFileSync(out, Buffer.from(data, "base64"));
    console.log(out);
} catch (err) {
    console.error(`web-screenshot: ${err.message}`);
    failed = true;
}
await send("browser.close").catch(() => {});
stop(failed ? 1 : 0);
