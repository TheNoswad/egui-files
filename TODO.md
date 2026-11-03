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
- [ ] mobile support (the layout runs on the desktop: View → Mobile layout; `src/app/mobile.rs`)
    - [x] tabs like a mobile browser
    - [x] Build for Android, with a phone's file access ("All files access"): `cargo apk build --release` in `android/` (Pixel 7a, Android 16)
    - [ ] iOS
    - [x] Touch: drag scrolls, a tap opens, a long press opens the context menu, and a long press then a drag selects (`src/touch.rs`)
    - [x] Touch in the trash and archive tabs: a tap opens (archive) or adds to the selection (trash, or the archive while selecting); back clears it
    - [x] Touch drag selection in the icon grid selects in reading order between the two items
    - [x] Moving files by touch: while selecting, a long press on a selected item drags the selection onto a folder
    - [x] Pinch to zoom: icon size in the grid, text size in the terminal (Ctrl+scroll in the mobile layout on the desktop)
    - [x] Open files in other apps: `ACTION_VIEW` with the file's media store `content://` URI (no FileProvider: the APK has no Java code); "Open with" shows Android's chooser. Files from archives extract to `Android/media/<package>` so they can be shared. Checked on the Pixel: a PDF in Firefox, text through the chooser, an image from a zip in Files by Google
    - [x] Trash tab's toolbar overflows at phone width
    - [x] Hide the bottom bar while the on-screen keyboard is up
    - [ ] Terminal good enough to develop on (on hold: researching first). Today it runs Android's own `sh` with toybox, so no git, compilers or package manager. Needs a Linux userland, and since Android 10 apps can't execute files they download into their own storage. Options:
        - proot + a downloaded distro rootfs (Debian, Alpine) in app storage, exec routed through proot's loader shipped as a native library (how UserLAnd and AnLinux do it). Real `apt`/`apk`; ptrace overhead on syscalls; a few hundred MB and a first-run setup
        - Termux's packages: fastest, but built for Termux's prefix (`/data/data/com.termux/files/usr`), so they'd need rebuilding and hosting ourselves
        - Hand off to an installed Termux (`RUN_COMMAND` intent): little work, but needs Termux and integrates shallowly
    - [ ] Predictive back: opt in (`enableOnBackInvokedCallback`) once the manifest can say so, then target API 36; targets 35 until then. cargo-apk 0.10 still can't write it, and opting in needs an `OnBackInvokedCallback`, which needs Java code in the APK
    - [x] Check on a real touchscreen that egui's own long touch and ours don't both fire
    - [x] Dialogs, the theme editor and the About window sized for a phone
    - [x] Haptic feedback (`src/haptics.rs`): a long press, each item a drag selection takes in, a pinch reaching its smallest or largest size, and a drop onto a folder. Checked on the Pixel (`dumpsys vibrator_manager`): the long press and the selection ticks; not yet the pinch or the drop
    - [ ] Haptics on Windows: `InputHapticsManager` (`Windows.Devices.Haptics`, SDK 10.0.28000, for haptic touchpads, mice and pens) once the `windows` crate has it (0.62.2 doesn't). Linux has no API for it

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
