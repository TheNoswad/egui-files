#!/bin/sh
# Puts the fonts for other scripts beside the demo's page. They're too big for
# the wasm or the repository (18 MB), and the page only fetches the ones it
# needs, so most visitors never download them. Each is checked against
# web/fonts.sha256, at a fixed commit of Google's Noto repositories.
#   web/fonts.sh <out-dir>
set -e
cd "$(dirname "$0")/.."
out=$1
cache=target/web-fonts
cjk=https://raw.githubusercontent.com/notofonts/noto-cjk/f8d157532fbfaeda587e826d4cd5b21a49186f7c
noto=https://raw.githubusercontent.com/notofonts/notofonts.github.io/e408cf09bf2042e9fe0d7d22789c29510e876359
mkdir -p "$cache" "$out"

fetch() {
    [ -f "$cache/$1" ] && return
    curl -fsSL -o "$cache/$1.part" "$2"
    mv "$cache/$1.part" "$cache/$1"
}

for region in SC JP KR; do
    fetch "NotoSans$region-Regular.otf" "$cjk/Sans/SubsetOTF/$region/NotoSans$region-Regular.otf"
done
for script in Armenian Hebrew Arabic Devanagari Bengali Tamil Thai Georgian; do
    fetch "NotoSans$script-Regular.ttf" "$noto/fonts/NotoSans$script/hinted/ttf/NotoSans$script-Regular.ttf"
done
fetch LICENSE-noto-cjk.txt "$cjk/Sans/LICENSE"
fetch LICENSE-noto-fonts.txt "$noto/fonts/LICENSE"

sums=$PWD/web/fonts.sha256
(cd "$cache" && sha256sum --quiet -c "$sums")
while read -r _ file; do
    cp "$cache/$file" "$out/$file"
done <"$sums"
