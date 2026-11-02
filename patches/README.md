# Patched crates

Copies of crates.io crates with a small fix each, wired in through
`[patch.crates-io]` in `Cargo.toml`. Everything else in them is the
published crate as-is, so a patch can be dropped once upstream fixes it.

## unrar_sys (0.5.8)

Only `build.rs`: it checks the host OS instead of the target, which breaks
cross-compiling to Windows.

## egui-winit (0.36.2)

Only `src/clipboard.rs`: every viewport (native window) shares one
`smithay_clipboard::Clipboard` per Wayland connection, kept for the life of
the process.

Upstream makes one per viewport and drops it when the window closes. That
destroys its event queue while the compositor may still be sending its data
device new `wl_data_offer`s, and the Wayland connection dies:

    not a valid new object id (4278190093), message data_offer(n)
    Error: WinitEventLoop(ExitFailure(1))

It reproduces in plain eframe 0.36.2: close one immediate viewport and open
another in the same frame, every second, while something changes the
clipboard (`wl-copy` in a loop). It crashes within seconds; with this patch
it doesn't. Tab windows hit it when a window's last tab is dragged out.

To update egui-winit: copy the new version from `~/.cargo/registry/src` over
this folder and redo the change in `init_smithay_clipboard` (and the two
places that lock it).
