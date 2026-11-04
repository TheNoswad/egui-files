# Patched crates

Copies of crates.io crates with a small fix each, wired in through
`[patch.crates-io]` in `Cargo.toml`. Everything else in them is the
published crate as-is, so a patch can be dropped once upstream fixes it.

## unrar_sys (0.5.8)

`build.rs`: it checks the host OS instead of the target, which breaks
cross-compiling to Windows. It also links `pthread`, which Android doesn't
have as a library of its own (it's in libc), and links no C++ library, which
Android needs named (`c++_static`).

`cpu_supports.hpp` (ours, force-included on Linux x86): unrar's
`__builtin_cpu_supports` links GCC's `__cpu_indicator_init` from the static
libgcc.a, which CachyOS builds for x86-64-v3 (BMI2), so the app died with
SIGILL before `main` on older CPUs (an Ivy Bridge i5-3230M). The header swaps in
a plain `cpuid` check; `archive::tests::gcc_cpu_detection_is_not_linked`
guards it.

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

Also passes the key press through when Ctrl+V finds no text on the clipboard,
instead of dropping it, so a picture can be pasted (`markdown::assets`).

## mermaid-rs-renderer (0.3.1)

Without its tests, benches and examples (2 MB), and their entries in
`Cargo.toml`. Three changes:

- `layout/mod.rs`, `layout/flowchart/edge_pipeline.rs`, `lib.rs`: `web_time`'s
  `Instant` on wasm32. Its timings call `std::time::Instant::now()`, which
  panics in the browser, on every diagram.
- `text_metrics.rs`: no font cache. Upstream writes a copy of the system font
  it measures with to `~/.cache/mmdr`, which a file manager shouldn't be doing.
- `text_metrics.rs`: `register_font`, to measure with the font diagrams are
  drawn with (`markdown::diagram`), on every platform, without scanning the
  installed fonts.

To update it: copy the new version from `~/.cargo/registry/src` over this
folder and redo these.
