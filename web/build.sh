#!/bin/sh
# Builds the web site into one folder to upload to any static host: the
# landing page, and the demo in demo/ (built for size, so it takes a while).
#   web/build.sh [out-dir]    (default target/site)
# Needs trunk (cargo install trunk --locked) and the wasm32-unknown-unknown
# target (rustup target add wasm32-unknown-unknown).
set -e
cd "$(dirname "$0")/.."
out=${1:-target/site}
mkdir -p "$out"
# Trunk empties demo/ itself. Its own target folder, so native builds don't
# rebuild after it.
CARGO_TARGET_DIR=target/wasm trunk build --release=true --cargo-profile web --public-url ./ \
    --dist "$out/demo" web/demo/index.html
web/demo-files.sh "$out/demo/files"
web/fonts.sh "$out/demo/fonts"
cp web/index.html web/screenshot-light.png web/screenshot-dark.png "$out/"
echo "Built $out. To try it: python3 -m http.server -d $out"
