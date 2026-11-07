#!/bin/sh
# Renders the app icon (assets/icon/egui-files.svg) for each platform; the
# results are committed, so builds don't need these tools.
#
#   scripts/icons.sh
#
# assets/icon/egui-files.png   512 px: the window's icon, and Linux's
# assets/icon/egui-files.ico   Windows' (16 to 256 px), put in the .exe by build.rs
# android/res/mipmap-*/        Android's launcher icon, at each density
#
# Needs rsvg-convert (librsvg) and ImageMagick.
set -eu
cd "$(dirname "$0")/.."
svg=assets/icon/egui-files.svg
tmp=target/icons
mkdir -p "$tmp"

rsvg-convert -w 512 -h 512 "$svg" -o assets/icon/egui-files.png

# Each size drawn from the SVG, not shrunk from a big one: sharper.
for size in 16 24 32 48 64 256; do
    rsvg-convert -w "$size" -h "$size" "$svg" -o "$tmp/$size.png"
done
magick "$tmp/16.png" "$tmp/24.png" "$tmp/32.png" "$tmp/48.png" "$tmp/64.png" \
    "$tmp/256.png" assets/icon/egui-files.ico

for density in mdpi:48 hdpi:72 xhdpi:96 xxhdpi:144 xxxhdpi:192; do
    dir=android/res/mipmap-${density%:*}
    mkdir -p "$dir"
    rsvg-convert -w "${density#*:}" -h "${density#*:}" "$svg" -o "$dir/ic_launcher.png"
done

if command -v oxipng >/dev/null; then
    oxipng -q -o 3 --strip safe assets/icon/egui-files.png android/res/mipmap-*/ic_launcher.png
fi
