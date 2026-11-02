# TODO

## General

- [ ] Video backend for Windows/macOS (WGL / ANGLE); Linux-only for now
- [ ] Zero-copy dmabuf → wgpu import for video, if readback ever matters
- [ ] Drag-out on X11: check by hand on a real X11 desktop with a window manager, into a file manager and a browser (tested on Xvfb against a minimal XDND target, including Escape and dragging back in). No drag cursor outside the window, since the pointer stays grabbed by winit
- [ ] Upstream: egui-winit (0.36 and 0.37) drops Ctrl+V entirely, without even the key press, when the clipboard has no text (or image), e.g. files copied in Explorer. We work around it with `file_clipboard::PasteKey` (Windows: `GetKeyState`) so copied files don't need a text fallback, which Discord would paste instead of the files. Report it, or send a PR that always passes the key press through (or adds an empty-paste event), then drop `PasteKey`.
- [ ] In-app title bar on Windows: check by hand that the DWM shadow shows, the snap layouts flyout opens over maximize (and maximize still works and highlights), and right-click opens the system menu (rounded corners confirmed on Windows 11, square when maximized or snapped, as native)
- [ ] In-app title bar on Wayland: right-click shows our own window menu, since eframe gives no way to reach winit's `show_window_menu` (the compositor's). Expose it (a viewport command upstream, or via our egui-winit patch) and use it there
- [ ] Upstream: egui-winit makes a smithay clipboard per viewport and drops it with the viewport, which kills the Wayland connection ("not a valid new object id … data_offer"); we patch it to share one (patches/README.md has a repro). Report it or send the PR, then drop the patch.
- [ ] Tab windows: check they keep updating while the main window is hidden (another workspace, minimized). Their UI runs in the main window's frame, and on Wayland winit may hold that frame until the compositor sends the main window a frame callback
- [ ] Tab windows: try dropping a tab outside the window, and into another of the app's windows, by hand on Wayland, X11 and Windows (only unit-tested so far; the drop is told from the pointer, which egui may lose as it leaves)
- [ ] sftp etc
- [ ] google drive
- [ ] proton drive
- [ ] mobile support
    - [ ] tabs like a mobile browser

## Proton Drive

Using [proton-sdk-rs](https://github.com/narrrl/proton-sdk-rs) (`proton-sdk` + `proton-drive-rs`;
unofficial, async on tokio, MSRV 1.96).

### Phase 1: account and read-only browsing

- [ ] Local-or-remote "location" layer for browser tabs and jobs (the app currently assumes `std::fs` / `PathBuf`)
- [ ] Tokio runtime on a background thread, talking to the UI over channels
- [ ] Login dialog: SRP password, TOTP; clear error for FIDO2-only accounts (not supported by the SDK)
- [ ] Store session tokens and key passphrases in the OS keyring (Secret Service / Credential Manager), not app state
- [ ] Resume the session without the password (`key_passphrases` → `with_key_passphrases`)
- [ ] Proton Drive entry under Places; folder listing; thumbnails
- [ ] Open files by downloading to a temp file (open with, archives)
- [ ] Preview via ranged reads (`open_revision` / `read_at`), video included

### Phase 2: writes

- [ ] Upload / download jobs, including drag in and drag out (via a temp file)
- [ ] New folder, rename, same-volume move, server-side copy
- [ ] Trash / restore / delete; remote trash tab
- [ ] Editor saves as a new revision
- [ ] Live updates from `EventManager` in place of file watching
- [ ] Quota display

### Phase 3: the rest

- [ ] Sharing: members, invites, public links (create / update / remove)
- [ ] Shared-with-me listing, incoming invites, bookmarks
- [ ] Revision history: browse, download, restore, delete
- [ ] Photos (timeline, albums)
- [ ] Open someone else's public link without an account
- [ ] Human verification (CAPTCHA) in a webview window (e.g. `wry`); the verify page posts its token to the embedding webview

### Constraints

- No moves between volumes (unsupported upstream)
- Proton's third-party rules: honest `x-pm-appversion`, event-based sync, no Proton branding
