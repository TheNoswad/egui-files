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
- [x] KDE Connect phones (Linux): paired, reachable phones are listed with the drives and mounted on a click through KDE Connect's D-Bus API. The app reaches the mount as a server's folder (`sftp://kdeconnect-<id>/…`) through a local `sftp-server`, and the UI's thread never waits on it (`disk::sftp::set_local_servers`, `src/phones.rs`). A phone that stops answering is cut off and mounted again once back; tabs on it stay and pick up again. Ejected with fusermount: KDE Connect's `unmount` leaves a dead mount that blocks the next one
    - [ ] Servers proper can still stall the UI's thread if it asks after something uncached while the connection hangs: `set_local_servers`'s rule isn't applied to them, as renames and new items there still run on that thread
    - [ ] Opening a bookmark or restored tab on a phone that isn't mounted doesn't mount it: it says it isn't connected until its row is clicked
- [x] SFTP: `sftp://user@host:port/path` in tabs, jobs, preview, editor and properties, through `disk` (`disk/src/sftp/`), with ssh doing the connecting; servers from `~/.ssh/config` and `known_hosts` in Places, the path bar and "Connect to server". Tried against OpenSSH 9.7 in a container, in the app under Xvfb, also with 150 ms each way
    - [ ] Android: there's no `ssh` there. russh (or similar) behind the same `Session`, with our own config parsing and auth
    - [ ] Windows: not built here (the C dependencies need MinGW). Check Windows' OpenSSH runs the app as `SSH_ASKPASS` (it's a GUI-subsystem program printing to the pipe ssh gives it), that no console flashes up, and that `File::set_permissions`/read-only map sensibly
    - [ ] Drag out of a server (copies, up to 16 MB) into another app, by hand: only built
    - [x] Others' changes on a server: the folders on screen are listed again every 10 s while connected
    - [x] Terminal: follows onto a server with ssh, and about it while the shell there says when it's at a prompt (OSC 133) and where (OSC 7). bash and fish get hooks with the ssh command; nothing's written on the server
    - [ ] zsh on a server is followed only if it marks its prompts itself (no way to hand it hooks without files there); tried with bash only by hand, fish in tests
    - [x] Copies within one server are done there (`copy-data`, OpenSSH 9.0+), 64 MB at a time for progress and cancelling
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
    - [x] tabs like a mobile browser
    - [x] Build for Android, with a phone's file access ("All files access"): `cargo apk build --release` in `android/` (Pixel 7a, Android 16)
    - [ ] iOS
    - [x] Touch: drag scrolls, a tap opens, a long press opens the context menu, and a long press then a drag selects (`src/touch.rs`)
    - [x] Touch in the trash and archive tabs: a tap opens (archive) or adds to the selection (trash, or the archive while selecting); back clears it
    - [x] Touch drag selection in the icon grid selects in reading order between the two items
    - [x] Moving files by touch: while selecting, a long press on a selected item drags the selection onto a folder
    - [x] Pinch to zoom: item size in the file views (also Ctrl+scroll on the desktop), text size in the terminal (Ctrl+scroll in the mobile layout on the desktop)
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
    - [ ] Moto G20 (Unisoc T700, Mali-G52, Android 11): noise at the bottom while scrolling with Vulkan, gone with "Disable HW overlays", so its display hardware mishandles the Vulkan driver's buffers. Settings → Appearance → Renderer → OpenGL is the workaround (checked on the Pixel that OpenGL runs); confirm it fixes the G20, and if so consider OpenGL by default on such phones
    - [ ] Haptics on Windows: `InputHapticsManager` (`Windows.Devices.Haptics`, SDK 10.0.28000, for haptic touchpads, mice and pens) once the `windows` crate has it (0.62.2 doesn't). Linux has no API for it

## Video editor

Trims and converts with the user's own ffmpeg, so the app ships no codecs (`src/clip/`). Android will use Media3 with the device's codecs instead: no pure Rust stack covers H.264 and H.265 yet, which is most of what phones film.

- [x] Editor tab: timeline with start and end handles, I and O, fast cut from a keyframe or frame-exact, formats, quality, what to keep, exports queued in the jobs bar
- [ ] Target size: guaranteed under the limit (two-pass, check, retry lower), warning first if it's impossibly small
- [ ] Resize and frame rate; audio: remove, choose a track, bitrate, audio only
- [ ] Hardware encoders, as a toggle when one is found
- [ ] Crop, rotate and flip, speed; GIF and animated WebP
- [ ] Settings field for ffmpeg's path, and offering to download an LGPL build (no x264 or x265 in those; none for macOS from BtbN)
- [ ] Windows and macOS: needs the video backend above; the editor stays off there until then
- [ ] Android: Media3 Transformer to export, ExoPlayer to show (getting its frames into egui needs a prototype)
- [ ] Try exporting from a phone's mount: the path is mapped, but no test or run has covered it

## Markdown

`src/markdown/`: `parse` (pulldown-cmark to blocks with source offsets), `layout` (blocks at a width, to a flat list of items), `view` (draws what's on screen, clicks), `images`, `links`, `drawing` (formulas and diagrams). The editor tab shows it in Source, Split and Preview. The source is the only document: anything that changes it is a splice of its bytes.

- [x] Renderer: CommonMark, GFM, footnotes, alerts and callouts, frontmatter, definition lists, sub/superscript, heading ids, wikilinks, highlights, tags, emoji shortcodes
- [x] Preview and Split, with scrolling kept together; Open shows WYSIWYG; the preview pane renders
- [x] Links followed in place with Back and Forward; programs are revealed rather than run
- [x] Local and remote pictures, SVG included; remote ones behind a setting, by default asked for each document
- [x] Phase 2, editing aids: formatting commands and shortcuts, lists that continue on Enter and nest on Tab, addresses pasted over text become links, table editing (`table.rs`), outline, word count and reading time, find in the rendered view, pasted and dropped pictures (`assets.rs`), a picture's lightbox
- [ ] Pasting files and pictures (`system_clipboard.rs`), in the file browser and documents: the whole path only tried on X11, and reading the clipboard on KWin Wayland; check Ctrl+V on Wayland and Windows. Android and the browser can't read it: there, pictures come by dropping or the toolbar's template
- [ ] Pinch-to-zoom in a picture's lightbox on a touchscreen is untested
- [x] Phase 3: math (`math.rs`, katex-native) and Mermaid (`diagram.rs`, mermaid-rs-renderer, patched: see `patches/README.md`), drawn as SVGs through the images store (`drawing.rs`)
- [ ] Formulas that define macros aren't drawn: an argument doubling itself reaches gigabytes in a second, past KaTeX's limit on expansions, and macros can hide nesting that overflows the stack, which can't be caught. To allow them, KaTeX needs limits on what expansions produce and on depth (`math::refusal`)
- [ ] Automatic equation numbers (`align`, `equation`) start over in each formula; `layout_numbered` can continue them, once a formula's key can depend on those before it
- [ ] Formulas and diagrams aren't found by find, and copy as spaces when selected: copy their source
- [ ] Non-Latin text in formulas (`\text{日本語}`) uses installed fonts, so boxes on the web
- [ ] `\ce` (mhchem) isn't in katex-rs; `\ce{H2O}` shows as an error
- [ ] A diagram can't be stopped once started: a big one keeps a core busy (flowcharts: 200 nodes, 15 s), and on the web stops the page, hence fewer lines allowed there (`diagram.rs`). A worker would fix both. Mindmaps overlap their nodes, and pie titles sit close to the pie: the crate's layout
- [x] Editing while reading: double-click a block in Preview to edit its markdown in place, with the formatting commands; a double-click below the last one adds a paragraph (`block_edit.rs`). One splice of the source per block, and one undo step
- [ ] Editing a list or quote is of the whole of it: smaller blocks, down to the item, would jump less
- [x] WYSIWYG: a mode editing the rendered text itself, with a cursor in the page (`wysiwyg.rs`, `editor/page.rs`). Each char knows the bytes it came from, every key is a splice of the source, and untouched bytes never change. What's typed is markdown as written, so `**` and `# ` work. Ctrl+click follows links; a double-click on what isn't text, like a formula, opens its markdown in place. Live view isn't needed
- [ ] WYSIWYG, what isn't editable on the page: callout titles and frontmatter (double-click them), and empty table cells, which have no place in the source to go by
- [ ] WYSIWYG: text being composed with an input method only shows once it's committed; check an IME, and the on-screen keyboard on Android and phones (only tried with a keyboard, on X11 and in Firefox)
- [x] WYSIWYG on touchscreens: a long press selects a word and dragging goes on from it, handles move each end, and a bar has Cut, Copy, Paste and All. A tap gives the cursor a handle, to drag it, or tap it for Paste at the cursor. On Android these use the system's clipboard, as does all copying in the app
- [ ] WYSIWYG: the setting for what "Edit" opens has no place in the settings to choose this mode (`Prefs::edit_mode`); find looks through the source there, as in Split
- [x] Phase 6: links in documents follow files moved or renamed in the app (`relink.rs`): the git repository or vault around the move is looked through, or without one the folders moved from and to, and what would change is shown first. Only link destinations' bytes change, and links already broken are left. Editor tabs go with their files
- [x] What's looked at when links update: raw HTML's `src` and `href` too, hidden folders but not tools' own (`.git`, `node_modules`), and documents up to 64 MiB; what can't be read is named in the list, as is what has unsaved changes
- [x] Repair links, in a document's toolbar: for what moved outside the app, which can't be told from a `git checkout`. Finds broken links around the document, and fixes those with one file of their name
- [x] `[[Note]]` goes by how the app follows it: a rename leaves links that meant another note of that name, and a name that would now find another note gets a path
- [ ] Raw HTML: a subset (img, br, details/summary, kbd, sub/sup, center); shown as written, dimmed, until then
- [ ] Export (HTML, rich text, PDF)
- [x] Scripts the bundled fonts lack, everywhere in the app: installed fonts on the desktop and Android, chosen by what they cover (`src/theme/fallback.rs`), and on the web Noto fonts beside the page, fetched when text needs them (`src/theme/web_fonts.rs`, `web/fonts.sh`)
- [ ] Right-to-left text (Hebrew, Arabic) isn't ordered: letters are shaped, but lines read left to right. egui has no bidi. See "Characters" in `samples/markdown/showcase.md`
- [ ] Emoji are monochrome, and flags don't work: egui has no color fonts
- [ ] Chinese characters come in regional forms, and the first font found wins (Chinese ones first), so Japanese text can get Chinese forms on the desktop. Prefer by the system's language
- [ ] Web: scripts with no font in `web/fonts.sh` stay boxes (Lao, Myanmar, Khmer, Tibetan, Ethiopic, and other Indian scripts), text in Japanese without kana gets the Chinese font, and these fonts have no bold
- [ ] Check fallback fonts on Windows and Android: only run on Linux and in Firefox so far (Android loads `/system/fonts` itself, as fontdb doesn't)
- [ ] Bare email addresses aren't linked (`<a@b.c>` is)
- [ ] Text selection in rendered documents on touchscreens (dragging scrolls there)
- [x] Web links, and tapping links and pictures, checked on a real Android device
- [x] Links in the preview pane: a heading scrolls the pane to it, and a program is shown in its folder
- [x] Links on a touchscreen while editing the page: a tap puts the cursor there, and a button by it opens the link, as there's no Ctrl

## Web demo

`web/build.sh` builds the site into `target/site`: the landing page (`web/index.html`), and the demo in `demo/` (`web/demo/`, built by trunk for wasm32-unknown-unknown, with the `web` profile, for size). The demo's files are in `web/demo-files/`: `web/demo-files.sh` puts them beside the page with a manifest, `src/demo.rs` lists them at startup, and a folder's files are fetched in the background when it's shown, into memory (`disk/`). clippy.toml warns, in the wasm build only, of what fails or panics in a browser.

- [x] Builds and starts in the browser; the terminal, video, libmpv, RAR and zstd say they need the desktop app
- [x] Files in memory, with a demo home: browsing, thumbnails, previews with highlighting, archives, copy/move/trash/restore, extracting
- [x] "Open" shows pictures, text, PDFs, audio and video in a browser tab, and saves anything else
- [x] A landing page with the demo embedded: it loads with the page behind a light or dark screenshot, with its progress, then fades in and grows to most of the window
- [x] Starts fresh in the home folder on every load, settings included; the mobile layout on a phone-sized page
- [x] Jobs share the page's thread: progress, cancelling, and conflict and password prompts work, and the page keeps drawing
- [x] Bring in files and folders, dropped on the page or through File → Add files/folder (1 GiB per job)
- [x] Fetch files from the server when they're first opened (`web/demo-files/`, listed in a manifest by `web/demo-files.sh`)
- [x] Fetch the shown folder's files in the background, smallest first, with pictures waiting for theirs: on a 300 KB/s connection, Pictures stopped the page for up to 10.5 s at a time, and now for 0.2 s
- [x] Size: 17.8 MB of wasm, 5.7 MB with brotli (11.5 and 4.3 before math and diagrams): built for size, WebGL through glow instead of wgpu, no persistence
- [x] Tried in Firefox: disk usage, compressing (zip, tar.xz, 7z with a password), and saving in the editor
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
