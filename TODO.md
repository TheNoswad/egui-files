# TODO

What's left, by area. What's done is in CHANGELOG.md; what we wait on
upstream is watched by `scripts/upstream.sh` (see `scripts/upstream.watch`).

## General

- [ ] Renaming the app: before it's announced, as installs on Android can't follow a new id (`org.noswad.egui_files`, android/Cargo.toml) and become a separate app. Also the window's app id and .desktop file (`packaging/linux/`), the icon's file names, download names (`scripts/package.sh`), the repository and its Pages address. Then the first official release, with the announcement: `scripts/release.sh 0.1.0` and push the tag (nightlies only until then), and the README's Downloads section points at releases again
- [ ] Open a folder given on the command line, so the .desktop file can say it opens folders (`MimeType=inode/directory`, `Exec=egui-files %U`) and the app can be the default file manager
- [ ] Try CI's downloads on real machines: so far only the Linux x86-64 AppImage has been. Not yet Windows (x86-64, arm64), Linux arm64 or the Android APKs; the README's Downloads section says so, and should change with this
- [ ] Video backend for macOS; Linux and Windows have one
- [ ] Zero-copy dmabuf → wgpu import for video, if readback ever matters
- [ ] Drag-out on X11: check by hand on a real X11 desktop with a window manager, into a file manager and a browser (tested on Xvfb against a minimal XDND target, including Escape and dragging back in). No drag cursor outside the window, since the pointer stays grabbed by winit
- [ ] Upstream: egui-winit (0.36 and 0.37) drops Ctrl+V entirely, without even the key press, when the clipboard has no text (or image), e.g. files copied in Explorer. We work around it with `file_clipboard::PasteKey` (Windows: `GetKeyState`) so copied files don't need a text fallback, which Discord would paste instead of the files. Not reported upstream; `scripts/upstream.sh` looks for a report or fix (an empty-paste event, or the key press always passed through), then drop `PasteKey`
- [ ] In-app title bar on Windows: check by hand that the DWM shadow shows, the snap layouts flyout opens over maximize (and maximize still works and highlights), and right-click opens the system menu (rounded corners confirmed on Windows 11, square when maximized or snapped, as native)
- [ ] In-app title bar on Wayland: right-click shows our own window menu, since eframe gives no way to reach winit's `show_window_menu` (the compositor's). Expose it (a viewport command upstream, or via our egui-winit patch) and use it there
- [ ] Upstream: egui-winit makes a smithay clipboard per viewport and drops it with the viewport, which kills the Wayland connection ("not a valid new object id … data_offer"); we patch it to share one (our egui fork's commit has a repro; see `Cargo.toml`). Not reported upstream; `scripts/upstream.sh` watches emilk/egui#8657, which may make it moot, then drop the patch
- [ ] egui 0.37: waits on egui_dock (anhosh/egui_dock#352, #354; watched by `scripts/upstream.sh`); then rebase our egui fork's branch onto 0.37 (both of its changes need redoing by hand: neither applies to 0.37 as it is) and update the revs in `Cargo.toml`
- [ ] Tab windows: check they keep updating while the main window is hidden (another workspace, minimized). Their UI runs in the main window's frame, and on Wayland winit may hold that frame until the compositor sends the main window a frame callback
- [ ] Tab windows: try dropping a tab outside the window, and into another of the app's windows, by hand on Wayland, X11 and Windows (only unit-tested so far; the drop is told from the pointer, which egui may lose as it leaves)
- KDE Connect phones (Linux):
    - [ ] Servers proper can still stall the UI's thread if it asks after something uncached while the connection hangs: `set_local_servers`'s rule isn't applied to them, as renames and new items there still run on that thread
    - [ ] Opening a bookmark or restored tab on a phone that isn't mounted doesn't mount it: it says it isn't connected until its row is clicked
- SFTP:
    - [ ] Android: there's no `ssh` there. russh (or similar) behind the same `Session`, with our own config parsing and auth
    - [ ] Windows: not built here (the C dependencies need MinGW). Check Windows' OpenSSH runs the app as `SSH_ASKPASS` (it's a GUI-subsystem program printing to the pipe ssh gives it), that no console flashes up, and that `File::set_permissions`/read-only map sensibly
    - [ ] Drag out of a server (copies, up to 16 MB) into another app, by hand: only built
    - [ ] zsh on a server is followed only if it marks its prompts itself (no way to hand it hooks without files there); tried with bash only by hand, fish in tests
    - [ ] FTP, WebDAV, SMB ("etc")
- [ ] google drive
- [ ] proton drive
- [ ] CSV viewer
- [ ] ISO file support
- [ ] PDF support
- [ ] Office file formats (open and proprietary)
- [ ] Any other format not supported yet
- [ ] Project focus or view mode, for IDE features
- [ ] IDE features
- [ ] mobile support (the layout runs on the desktop: View → Mobile layout; `src/app/mobile.rs`)
    - [ ] iOS
    - [ ] Terminal good enough to develop on (on hold: researching first). Today it runs Android's own `sh` with toybox, so no git, compilers or package manager. Needs a Linux userland, and since Android 10 apps can't execute files they download into their own storage. Options:
        - proot + a downloaded distro rootfs (Debian, Alpine) in app storage, exec routed through proot's loader shipped as a native library (how UserLAnd and AnLinux do it). Real `apt`/`apk`; ptrace overhead on syscalls; a few hundred MB and a first-run setup
        - Termux's packages: fastest, but built for Termux's prefix (`/data/data/com.termux/files/usr`), so they'd need rebuilding and hosting ourselves
        - Hand off to an installed Termux (`RUN_COMMAND` intent): little work, but needs Termux and integrates shallowly
    - [ ] Predictive back: opt in (`enableOnBackInvokedCallback`) once the manifest can say so, then target API 36; targets 35 until then. cargo-apk 0.10 still can't write it, and opting in needs an `OnBackInvokedCallback`, which needs Java code in the APK
    - [ ] Moto G20 (Unisoc T700, Mali-G52, Android 11): noise at the bottom while scrolling with Vulkan, gone with "Disable HW overlays", so its display hardware mishandles the Vulkan driver's buffers. Settings → Appearance → Renderer → OpenGL is the workaround (checked on the Pixel that OpenGL runs); confirm it fixes the G20, and if so consider OpenGL by default on such phones
    - [ ] Haptics on Windows: `InputHapticsManager` (`Windows.Devices.Haptics`, SDK 10.0.28000, for haptic touchpads, mice and pens) once the `windows` crate has it (0.62.2 doesn't). Linux has no API for it

## Video editor

Trims and converts with the user's own ffmpeg, so the app ships no codecs (`src/clip/`). Android uses its own MediaCodec and MediaMuxer classes instead: no pure Rust stack covers H.264 and H.265 yet, which is most of what phones film.

- Target size limits:
    - [ ] The 0.005 limit was judged from one frame of one recording, and the hardware floors (6.5 kbit a frame for H.264, 4.5 for others) were measured on one AMD card with game footage. Busier video needs more, which the early stop catches only after a try
- Speed:
    - [ ] NVENC and QuickSync have no such GPU path (`-hwaccel cuda`/`qsv`), nor Windows
    - [ ] "Fast" is only measured for x264; check x265, SVT-AV1, VP9 and AOM on real footage (VP9's faster setting was slower on noise)
- [ ] Target size for audio on its own (only the bitrate can be chosen there)
- [ ] Windows on real hardware: only run under Wine so far, with no GPU. Check hardware decoding (`auto-copy-safe`), hardware encoders, high DPI and a 4K video
- [ ] macOS: not planned for now, nothing here to build or run it on
- [ ] Before shipping publicly, decide what the ffmpeg and libmpv downloads fetch. They take the newest third-party build, checked only against a hash from the same place; libmpv's build is GPL and its releases last about 6 weeks. The recommendation is our own LGPL builds on this project's releases, with hashes pinned in the app
- [ ] Android: faster re-encoding. On the Pixel 7a most of the time is copying frames out of the decoder (about 12 ms a 4K frame at the same size, 26 ms resizing), not the codecs. Tried, no help: codec `operating-rate`/`priority`, the scaler on 4 threads (slower), reading rows in bulk
    - [ ] Same-size exports: the decoder renders straight into the encoder's input Surface (`createInputSurface`, `releaseOutputBuffer(index, time)`), no frame touching the processor and no Java needed. HDR to SDR needs checking there
    - [ ] Resizing (and crop, turn, mirror): draw between the two Surfaces with OpenGL ES (EGL from the NDK, a SurfaceTexture polled with `updateTexImage`)
- [ ] Android: GIF and animated WebP, written in Rust as Android has no encoder for them, with a target-size loop like desktop's
- [ ] Android: other audio formats. WAV is simple and Opus files should be, as the phone can encode Opus; MP3 and FLAC take more
- [ ] Android: keep the title, date and subtitles when exporting, which the muxer has nowhere for
- [ ] Flaky test: `editor::tests::a_link_leaves_unsaved_changes_in_their_tab` failed once in the pre-commit hook on 2026-11-04, in a slow run with ffmpeg busy elsewhere. About 50 runs since passed, some pinned to one or two cores. The failing assertion wasn't captured, so the cause isn't known
- [ ] Android: listen to speed changes on real sound (`/sdcard/bird (edited) (2).mp4` on the Pixel): so far only pitch on a pure tone and loudness are measured
- [ ] Android: exports that keep going in the background: needs a foreground service, so a Java class, which cargo-apk can't build
- [ ] Try exporting from a phone's mount: the path is mapped, but no test or run has covered it
- Stills along the timeline (`strip.rs`):
    - [ ] Android: the file views show no video thumbnails, as there's no ffmpeg; `ffmpeg::stills` could make them there
- Volume:
    - [ ] Android: mixing tracks (two decoders kept in step, their samples added) and evening out loudness (`dynaudnorm`'s idea in Rust); both are hidden there until then
    - [ ] Raising a loud track can clip: a limiter after the volume, if it turns out to be a problem

## Markdown

`src/markdown/`: `parse` (pulldown-cmark to blocks with source offsets), `layout` (blocks at a width, to a flat list of items), `view` (draws what's on screen, clicks), `images`, `links`, `drawing` (formulas and diagrams). The editor tab shows it in Source, Split and Preview. The source is the only document: anything that changes it is a splice of its bytes.

- [ ] Pasting files and pictures (`system_clipboard.rs`), in the file browser and documents: the whole path only tried on X11, and reading the clipboard on KWin Wayland; check Ctrl+V on Wayland and Windows. Android and the browser can't read it: there, pictures come by dropping or the toolbar's template
- [ ] Pinch-to-zoom in a picture's lightbox on a touchscreen is untested
- [ ] Raw HTML shows as written (`BlockKind::Html`), so READMEs' screenshots and badges come out as source, ours included. Most of it is drawn now. Still to come: simple `<table>`s and `<details>`
- [ ] Formulas that define macros aren't drawn: an argument doubling itself reaches gigabytes in a second, past KaTeX's limit on expansions, and macros can hide nesting that overflows the stack, which can't be caught. To allow them, KaTeX needs limits on what expansions produce and on depth (`math::refusal`)
- [ ] Automatic equation numbers (`align`, `equation`) start over in each formula; `layout_numbered` can continue them, once a formula's key can depend on those before it
- [ ] Formulas and diagrams aren't found by find, and copy as spaces when selected: copy their source
- [ ] Non-Latin text in formulas (`\text{日本語}`) uses installed fonts, so boxes on the web
- [ ] `\ce` (mhchem) isn't in katex-rs; `\ce{H2O}` shows as an error
- [ ] A diagram can't be stopped once started: a big one keeps a core busy (flowcharts: 200 nodes, 15 s), and on the web stops the page, hence fewer lines allowed there (`diagram.rs`). A worker would fix both. Mindmaps overlap their nodes, and pie titles sit close to the pie: the crate's layout
- [ ] Editing a list or quote is of the whole of it: smaller blocks, down to the item, would jump less
- [ ] WYSIWYG, what isn't editable on the page: callout titles and frontmatter (double-click them), and empty table cells, which have no place in the source to go by
- [ ] WYSIWYG: text being composed with an input method only shows once it's committed; check an IME, and the on-screen keyboard on Android and phones (only tried with a keyboard, on X11 and in Firefox)
- [ ] WYSIWYG: the setting for what "Edit" opens has no place in the settings to choose this mode (`Prefs::edit_mode`); find looks through the source there, as in Split
- [ ] Raw HTML: blocks with details/summary or tables are shown as written, dimmed, until they're drawn
- [ ] Export (HTML, rich text, PDF)
- [ ] Right-to-left text (Hebrew, Arabic) isn't ordered: letters are shaped, but lines read left to right. egui has no bidi. See "Characters" in `samples/markdown/showcase.md`
- [ ] Emoji are monochrome, and flags don't work: egui has no color fonts
- [ ] Chinese characters come in regional forms, and the first font found wins (Chinese ones first), so Japanese text can get Chinese forms on the desktop. Prefer by the system's language
- [ ] Web: scripts with no font in `web/fonts.sh` stay boxes (Lao, Myanmar, Khmer, Tibetan, Ethiopic, and other Indian scripts), text in Japanese without kana gets the Chinese font, and these fonts have no bold
- [ ] Check fallback fonts on Windows and Android: only run on Linux and in Firefox so far (Android loads `/system/fonts` itself, as fontdb doesn't)
- [ ] Bare email addresses aren't linked (`<a@b.c>` is)
- [ ] Text selection in rendered documents on touchscreens (dragging scrolls there)

## Web demo

`web/build.sh` builds the site into `target/site`: the landing page (`web/index.html`), and the demo in `demo/` (`web/demo/`, built by trunk for wasm32-unknown-unknown, with the `web` profile, for size). The demo's files are in `web/demo-files/`: `web/demo-files.sh` puts them beside the page with a manifest, `src/demo.rs` lists them at startup, and a folder's files are fetched in the background when it's shown, into memory (`disk/`). clippy.toml warns, in the wasm build only, of what fails or panics in a browser.

- [ ] Publishing (GitHub Pages or Cloudflare Pages), from CI; the site is static, with relative URLs. Check the host compresses the wasm, and serves the dotfiles in `demo/files/` (`.profile`, `.gitignore`)
- [ ] Better content for `web/demo-files/`: real photos and such, instead of pictures drawn by code. `Projects/egui-files` is a copy of some of the source, and `Downloads/` packs some of these files, so they don't follow changes
- [ ] Opening a file before its background fetch is done stops the page until it's here, and downloads it twice (a synchronous request, as its readers can't wait): 10 s for a 2.9 MB photo at 300 KB/s. So does a job or an editor reaching a file from a folder that wasn't shown. Have "Open" wait for the fetch, as pictures do. Showing a folder also fetches everything in it, so a big video in `web/demo-files/` would cost every visitor who opens that folder: skip big files then, or use Range requests
- [ ] A job still stops the page inside one archive entry: extracting pauses between entries of zip and tar, compressing between files of zip and tar, and 7z not at all (its crate's callback can't wait)
- [ ] Try folders brought in for real: dropped folders and the folder dialog were only checked with stand-ins, as WebDriver can't provide either
- [ ] About's credits list the native build's crates (unrar, alacritty_terminal), not the web build's
- [ ] Links on the landing page to the source and downloads, once the repo has a remote
- [ ] Try the demo on real phones: touch (only checked with a mouse), whether the keyboard opens for rename and the path bar, that Add files opens a picker, and that Firefox's `devicePixelContentBoxSize` workaround in `web/demo/index.html` stays out of the way (checked only in emulation)
- [ ] Try it in other browsers (only Firefox so far). "Open with" icons still `.expect` a thread (`open_with::icon_texture`), if anything ever asks for one there
- [ ] Check the file list's background on the Pixel: the mobile layout's middle had no fill, which showed nearly black in the browser (fixed there; not looked at on Android)

## Proton Drive

Using [proton-sdk-rs](https://github.com/narrrl/proton-sdk-rs) (`proton-sdk` + `proton-drive-rs`;
unofficial, async on tokio, MSRV 1.96).

### Phase 1: account and read-only browsing

- [ ] Local-or-remote "location" layer for browser tabs and jobs: SFTP made one, in `disk` (paths with a scheme, `std::fs`-like types that wrap either), and `remote::listing` for listing off the UI's thread; Proton Drive could stand behind the same
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
