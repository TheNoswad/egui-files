# TODO

## General

- [ ] Settings screen / config: columns, keybindings, sidebar sections, default view
- [ ] Video backend for Windows/macOS (WGL / ANGLE); Linux-only for now
- [ ] Zero-copy dmabuf → wgpu import for video, if readback ever matters
- [ ] Conflict dialog (overwrite / skip) for paste
- [ ] Drag-out on X11 (Wayland and Windows done)
- [ ] Disk usage: keep scans after their tab closes?
- [ ] Folder sizes: skip (or ask before) measuring network and FUSE mounts
- [ ] Upstream: egui-winit (0.36 and 0.37) drops Ctrl+V entirely, without even the key press, when the clipboard has no text (or image), e.g. files copied in Explorer. We work around it with `file_clipboard::PasteKey` (Windows: `GetKeyState`) so copied files don't need a text fallback, which Discord would paste instead of the files. Report it, or send a PR that always passes the key press through (or adds an empty-paste event), then drop `PasteKey`.
- [ ] Custom titlebar that looks native (opt-in; the system frame stays the default)
    - [ ] Undecorated window: drag to move (`StartDrag`), double-click to maximize, resize from the edges (`BeginResize`), and the system window menu on right-click where there is one
    - [ ] File / View / Help inline at its left, in place of the menu bar row
    - [ ] Window buttons: settings for the side (left / right), which ones (minimize, maximize, close) and their look (Windows, Adwaita, Breeze, macOS); default to the platform's, and on Linux read the GNOME / KDE button layout
    - [ ] Optional parts, each with a toggle: title (the focused tab's folder), back / forward / up for the focused tab, a path or search box, pane toggles (sidebar, preview, terminal). Defaults: title and navigation on, the rest off
    - [ ] Keep enough empty space to drag by, however much is turned on
    - [ ] Windows: rounded corners and the shadow via DWM; the Win11 snap layouts flyout needs `HTMAXBUTTON` from `WM_NCHITTEST`, which winit doesn't give us
    - [ ] Settings section for all of the above, next to the settings screen
- [ ] properties tab should be a tab style window?
- [ ] option to set default sort type
- [ ] scaling issues with the treemap view
- [ ] sftp etc
- [ ] google drive
- [ ] proton drive
- [ ] mobile support
    - [ ] tabs like a mobile browser
- [ ] exe information?

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
