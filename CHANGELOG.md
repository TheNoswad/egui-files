# Changelog

What's done, by area; what isn't is in TODO.md. There have been no
releases yet, so it's all unreleased.

## Unreleased

### General

- Camera raw photos (CR2, CR3, NEF, NRW, ARW, DNG, RAF, ORF, RW2, PEF, SRW; `raw.rs`): thumbnails and the preview show the JPEG the camera stored in the file, turned as it should be and labelled with its size, and thumbnails are kept in the shared cache. Properties has the camera details and the photo's real size. Only the parts of the file that are needed are read. Files with no JPEG in them, like some phones' DNGs, say so
- KDE Connect phones (Linux): paired, reachable phones are listed with the drives and mounted on a click through KDE Connect's D-Bus API. The app reaches the mount as a server's folder (`sftp://kdeconnect-<id>/…`) through a local `sftp-server`, and the UI's thread never waits on it (`disk::sftp::set_local_servers`, `src/phones.rs`). A phone that stops answering is cut off and mounted again once back; tabs on it stay and pick up again. Ejected with fusermount: KDE Connect's `unmount` leaves a dead mount that blocks the next one
- SFTP: `sftp://user@host:port/path` in tabs, jobs, preview, editor and properties, through `disk` (`disk/src/sftp/`), with ssh doing the connecting; servers from `~/.ssh/config` and `known_hosts` in Places, the path bar and "Connect to server". Tried against OpenSSH 9.7 in a container, in the app under Xvfb, also with 150 ms each way
    - Others' changes on a server: the folders on screen are listed again every 10 s while connected
    - Terminal: follows onto a server with ssh, and about it while the shell there says when it's at a prompt (OSC 133) and where (OSC 7). bash and fish get hooks with the ssh command; nothing's written on the server
    - Copies within one server are done there (`copy-data`, OpenSSH 9.0+), 64 MB at a time for progress and cancelling
- mobile support (the layout runs on the desktop: View → Mobile layout; `src/app/mobile.rs`):
    - tabs like a mobile browser
    - Build for Android, with a phone's file access ("All files access"): `cargo apk build --release` in `android/` (Pixel 7a, Android 16)
    - Touch: drag scrolls, a tap opens, a long press opens the context menu, and a long press then a drag selects (`src/touch.rs`)
    - Touch in the trash and archive tabs: a tap opens (archive) or adds to the selection (trash, or the archive while selecting); back clears it
    - Touch drag selection in the icon grid selects in reading order between the two items
    - Moving files by touch: while selecting, a long press on a selected item drags the selection onto a folder
    - Pinch to zoom: item size in the file views (also Ctrl+scroll on the desktop), text size in the terminal (Ctrl+scroll in the mobile layout on the desktop)
    - Open files in other apps: `ACTION_VIEW` with the file's media store `content://` URI (no FileProvider: the APK has no Java code); "Open with" shows Android's chooser. Files from archives extract to `Android/media/<package>` so they can be shared. Checked on the Pixel: a PDF in Firefox, text through the chooser, an image from a zip in Files by Google
    - Trash tab's toolbar overflows at phone width
    - Hide the bottom bar while the on-screen keyboard is up
    - Check on a real touchscreen that egui's own long touch and ours don't both fire
    - Dialogs, the theme editor and the About window sized for a phone
    - Haptic feedback (`src/haptics.rs`): a long press, each item a drag selection takes in, a pinch reaching its smallest or largest size, and a drop onto a folder. Checked on the Pixel (`dumpsys vibrator_manager`): the long press and the selection ticks; not yet the pinch or the drop

- Screenshots taken by the app itself (`scripts/screenshots.sh`, `src/scene.rs`): set scenes shot on a virtual X server with the demo's files as a home at `/home/demo`, in light and dark, for the README; the landing page's from the web demo in headless Firefox
- Downloads, built by CI on Linux for every platform: AppImage and tar.gz for Linux (x86-64, arm64), zips for Windows (x86-64, arm64, through llvm-mingw), APKs for Android (arm64, x86-64), signed. Version tags make releases (`scripts/release.sh`), and main a nightly; the web demo goes to GitHub Pages
- An icon: Phosphor's "folders" glyph on a blue square (`assets/icon/`, rendered by `scripts/icons.sh`), in the window, the .exe, the Linux .desktop file and the Android launcher. The Android app id is now `org.noswad.egui_files`
- `scripts/upstream.sh`: what we wait on upstream, read-only, and whether our forks' branches need rebasing onto a newer release

### Video editor

- Editor tab: timeline with start and end handles, I and O, fast cut from a keyframe or frame-exact, formats, quality, what to keep, exports queued in the jobs bar
- Target size: under the limit (two passes where the encoder has them, check, retry lower), refused if it's impossibly small
- Target size limits: refused below 0.005 bits a pixel (frames past 60 fps not counted) and below hardware encoders' least bits a frame, with a button for the largest picture and frame rate that fits (or the software encoder); the container's index is in the budget; a try stops as soon as its file is over, and tries stop when the encoder goes no lower. Checked on a 10 minute 1440p 165 fps game recording
- Speed: VAAPI decodes and resizes on the GPU when nothing needs the processor (falls back on its own if decoding fails), frames are dropped before anything else, and an "Encoding speed" choice for software encoders. On that recording: a hardware resize to 720p went from 31 s to 12 s a minute, a software 480p at 30 fps from 29 s to 16 s
- Resize and frame rate; audio: remove, choose a track, bitrate, audio only
- Hardware encoders, as a toggle when one is found (run here: VAAPI; NVENC and QuickSync only have their arguments tested)
- Crop (free or to a shape), turn and mirror, speed; GIF and animated WebP, also to a target size
- Turning, mirroring and speed show in the editor's video, with the crop's rectangle turning along
- Settings field for ffmpeg's path, and offering to download an LGPL build (no x264 or x265 in those) and, on Windows, libmpv
- Windows: the player and editor, run under Wine with the downloaded tools and the export tests
- Android: re-encoding with MediaCodec: frame-exact, target size, quality, resolution, frame rate, H.264, H.265, AV1, VP9, AAC and Opus (`src/clip/ffmpeg/android_encode.rs`). Frames go through memory and are scaled in Rust, so there's no OpenGL or Java. Checked on a Pixel 7a
- Android: crop, turn and mirror (drawn in `scale.rs`, compared with ffmpeg's result at each stored rotation)
- Android: speed. Frames get new times, and `tempo.rs` changes the audio without changing its pitch (WSOLA), as ffmpeg's atempo does on desktop
- Android: long exports. A wake lock keeps the screen on (the window flag hung the app). Leaving the app, Android slows the export and freezes it after about 70 s on a Pixel 7a; it carries on, intact, on returning. The editor says so
- Editor layout: sections with a footer that keeps Export in view, a video box shaped like the picture, a ruler, typed start and end times, frame steps and jumps, zooming, and a narrow layout down to about 260 pt. A blocked Export says why on a click and points at the setting; a target size too small (or too small to look good) offers one-click fixes
- Scrubbing: no playhead jump (mpv's old position is ignored until the seek is done), a fine drag at a tenth of the speed (Shift, or the pointer away from the bar), and Settings → Video → "Exact frames while scrubbing" for keyframe seeks while dragging
    - Android: the fine drag checked on the Pixel 7a: 300 px away from the bar moved 0.28 s, along it 2.8 s
- Stills along the timeline (`strip.rs`): the whole video thinly first, then finer where it's zoomed into, down to a frame, from a grid of halves of its length so panning finds the same ones. Two ffmpeg runs at a time on the desktop; one `MediaMetadataRetriever` on Android. What's trimmed off is dimmed
    - Android: checked on the Pixel 7a: stills come within a few seconds, upright for videos stored turned 90° and 180° (the retriever turns them itself; `android_logic::upright` is only a fallback)
- Volume: a slider for each track kept (0 to 400%), "Mix into one track", as most players and websites play only the first, and "Even out loudness" (`dynaudnorm`), for inputs already mixed into one track. Changing the sound re-encodes only the audio; the video can still be copied. Checked by measuring exports: 400% is +12 dB, mixing makes one track
    - Android: the volume, applied to the samples in Rust on their way to the encoder (checked on the Pixel 7a: 200% came out +6.0 dB)

### Markdown

- Renderer: CommonMark, GFM, footnotes, alerts and callouts, frontmatter, definition lists, sub/superscript, heading ids, wikilinks, highlights, tags, emoji shortcodes
- Preview and Split, with scrolling kept together; Open shows WYSIWYG; the preview pane renders
- Links followed in place with Back and Forward; programs are revealed rather than run
- Raw HTML in a line of text is drawn, through html5gum (`src/markdown/html.rs`): text styles, `kbd`, `sub`, `sup`, `br`, links and `id` anchors, and `img` with `width` and `height`. Comments and `script`, `style` and the like don't show; other tags go and their content stays. In WYSIWYG the tags are hidden like `**`, and a block of HTML is one unit, double-clicked to edit its source
- Raw HTML's blocks: `div`, `p`, `center` and `align`, headings, lists, quotes, `pre`, `hr`, with markdown between tags (blank lines around it) as it is on GitHub. `<picture>` and `#gh-dark-mode-only` images follow the app's theme, and `<img align>` sets an image to one side, on its own line
- Raw HTML's `<details>` (opening and closing never changes the file) and tables (`th`, `align`, `colspan`, and `rowspan` ignored). Its headings join the outline, stats count what shows, and link updates read HTML with the same parser, `srcset` included
- Local and remote pictures, SVG included; remote ones behind a setting, by default asked for each document
- Phase 2, editing aids: formatting commands and shortcuts, lists that continue on Enter and nest on Tab, addresses pasted over text become links, table editing (`table.rs`), outline, word count and reading time, find in the rendered view, pasted and dropped pictures (`assets.rs`), a picture's lightbox
- Phase 3: math (`math.rs`, katex-native) and Mermaid (`diagram.rs`, mermaid-rs-renderer, patched: see `[patch.crates-io]` in `Cargo.toml`), drawn as SVGs through the images store (`drawing.rs`)
- Editing while reading: double-click a block in Preview to edit its markdown in place, with the formatting commands; a double-click below the last one adds a paragraph (`block_edit.rs`). One splice of the source per block, and one undo step
- WYSIWYG: a mode editing the rendered text itself, with a cursor in the page (`wysiwyg.rs`, `editor/page.rs`). Each char knows the bytes it came from, every key is a splice of the source, and untouched bytes never change. What's typed is markdown as written, so `**` and `# ` work. Ctrl+click follows links; a double-click on what isn't text, like a formula, opens its markdown in place. Live view isn't needed
- WYSIWYG on touchscreens: a long press selects a word and dragging goes on from it, handles move each end, and a bar has Cut, Copy, Paste and All. A tap gives the cursor a handle, to drag it, or tap it for Paste at the cursor. On Android these use the system's clipboard, as does all copying in the app
- Phase 6: links in documents follow files moved or renamed in the app (`relink.rs`): the git repository or vault around the move is looked through, or without one the folders moved from and to, and what would change is shown first. Only link destinations' bytes change, and links already broken are left. Editor tabs go with their files
- What's looked at when links update: raw HTML's `src` and `href` too, hidden folders but not tools' own (`.git`, `node_modules`), and documents up to 64 MiB; what can't be read is named in the list, as is what has unsaved changes
- Repair links, in a document's toolbar: for what moved outside the app, which can't be told from a `git checkout`. Finds broken links around the document, and fixes those with one file of their name
- `[[Note]]` goes by how the app follows it: a rename leaves links that meant another note of that name, and a name that would now find another note gets a path
- Scripts the bundled fonts lack, everywhere in the app: installed fonts on the desktop and Android, chosen by what they cover (`src/theme/fallback.rs`), and on the web Noto fonts beside the page, fetched when text needs them (`src/theme/web_fonts.rs`, `web/fonts.sh`)
- Web links, and tapping links and pictures, checked on a real Android device
- Links in the preview pane: a heading scrolls the pane to it, and a program is shown in its folder
- Links on a touchscreen while editing the page: a tap puts the cursor there, and a button by it opens the link, as there's no Ctrl

### Web demo

- Builds and starts in the browser; the terminal, video, libmpv, RAR and zstd say they need the desktop app
- Files in memory, with a demo home: browsing, thumbnails, previews with highlighting, archives, copy/move/trash/restore, extracting
- "Open" shows pictures, text, PDFs, audio and video in a browser tab, and saves anything else
- A landing page with the demo embedded: it loads with the page behind a light or dark screenshot, with its progress, then fades in and grows to most of the window
- Starts fresh in the home folder on every load, settings included, with its README selected and previewed; the mobile layout on a phone-sized page
- Jobs share the page's thread: progress, cancelling, and conflict and password prompts work, and the page keeps drawing
- Bring in files and folders, dropped on the page or through File → Add files/folder (1 GiB per job)
- Fetch files from the server when they're first opened (`web/demo-files/`, listed in a manifest by `web/demo-files.sh`)
- Fetch the shown folder's files in the background, smallest first, with pictures waiting for theirs: on a 300 KB/s connection, Pictures stopped the page for up to 10.5 s at a time, and now for 0.2 s
- Size: 17.8 MB of wasm, 5.7 MB with brotli (11.5 and 4.3 before math and diagrams): built for size, WebGL through glow instead of wgpu, no persistence
- Tried in Firefox: disk usage, compressing (zip, tar.xz, 7z with a password), and saving in the editor
