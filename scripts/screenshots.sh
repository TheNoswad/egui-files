#!/bin/sh
# Takes the screenshots: the README's, into docs/screenshots/, and the landing
# page's stand-in for the web demo while it loads (web/screenshot-*.png). Each
# in the light theme and the dark.
#
#   scripts/screenshots.sh [shot...]     all of them if none are named
#
# The shots: markdown, split, archive and phone, scenes of src/scene.rs shot
# by the desktop app itself on a virtual X server; and web, the web demo as it
# starts, shot in headless Firefox. Both have the demo's files
# (web/demo-files/) as their home.
#
# The desktop app sees its home at /home/demo, in a mount namespace of its
# own (so nothing outside it changes), which needs unprivileged user
# namespaces. It needs Xvfb (on PATH, or XVFB=/path/to/Xvfb), Mesa's software
# OpenGL, util-linux's unshare, and bash for the terminal. The web demo needs
# trunk, python3, node and firefox. oxipng, if it's there, makes the files
# smaller. Slow machines can give each shot longer to settle with
# EGUI_FILES_SCREENSHOT_AFTER=<seconds> (6 by default).
set -eu
cd "$(dirname "$0")/.."

shots=${*:-markdown split archive phone web}
desktop=
web=
for shot in $shots; do
    case $shot in
        markdown | split | archive | phone) desktop="$desktop $shot" ;;
        web) web=1 ;;
        *)
            echo "screenshots.sh: no shot $shot (markdown split archive phone web)" >&2
            exit 2
            ;;
    esac
done
work=$PWD/target/screenshots
mkdir -p "$work"
cleanup=:
trap 'eval "$cleanup"' EXIT
trap 'exit 1' INT TERM

needs() {
    for tool in "$@"; do
        command -v "$tool" >/dev/null || {
            echo "screenshots.sh: needs $tool" >&2
            exit 1
        }
    done
}

shrink() {
    if command -v oxipng >/dev/null; then
        oxipng -q -o 3 --strip safe "$1"
    fi
}

shoot_desktop() {
    xvfb=${XVFB:-Xvfb}
    needs "$xvfb" unshare bash
    out=docs/screenshots
    mkdir -p "$out"
    cargo build --release --quiet
    app=$PWD/target/release/egui-files

    # The home folder: the demo's files, dated as web/demo-files.sh dates
    # them (by name, up to 45 days back), so they look lived in.
    rm -rf target/screenshots/home target/screenshots/xdg
    mkdir -p "$work/home"
    cp -RP web/demo-files/. "$work/home/"
    now=$(date +%s)
    (
        cd "$work/home"
        find . -mindepth 1 ! -type l | sed 's|^\./||' | while IFS= read -r file; do
            sum=$(printf '%s' "$file" | cksum | cut -d' ' -f1)
            touch -d "@$((now - sum % (45 * 86400)))" "$file"
        done
    )
    # A prompt, and a terminal title, that name no real user or machine.
    cat >"$work/home/.bashrc" <<'EOF'
PS1='\[\e]0;demo@laptop: \w\a\e[1;32m\]demo@laptop\[\e[0m\]:\[\e[1;34m\]\w\[\e[0m\]\$ '
EOF
    cat >"$work/home/.bash_profile" <<'EOF'
. ~/.profile
. ~/.bashrc
EOF

    # The usual folders, for the places pane.
    mkdir -p "$work/xdg/config"
    cat >"$work/xdg/config/user-dirs.dirs" <<'EOF'
XDG_DOCUMENTS_DIR="$HOME/Documents"
XDG_DOWNLOAD_DIR="$HOME/Downloads"
XDG_MUSIC_DIR="$HOME/Music"
XDG_PICTURES_DIR="$HOME/Pictures"
EOF

    stage=$(mktemp -d)
    cleanup="$cleanup; rmdir '$stage'"
    as_demo true || {
        echo "screenshots.sh: can't make a user namespace (see unshare(1))" >&2
        exit 1
    }

    # A display nobody's using.
    n=90
    while [ -e "/tmp/.X11-unix/X$n" ] || [ -e "/tmp/.X$n-lock" ]; do
        n=$((n + 1))
    done
    "$xvfb" ":$n" -screen 0 2560x2560x24 -nolisten tcp >"$work/xvfb.log" 2>&1 &
    cleanup="$cleanup; kill $! 2>/dev/null || true"
    tries=0
    until [ -e "/tmp/.X11-unix/X$n" ]; do
        tries=$((tries + 1))
        if [ "$tries" -gt 50 ]; then
            echo "screenshots.sh: Xvfb didn't start (see $work/xvfb.log)" >&2
            exit 1
        fi
        sleep 0.1
    done

    for scene in $desktop; do
        # A phone's pixels to the point, as a Pixel has.
        scale=1
        [ "$scene" != phone ] || scale=2.625
        for theme in light dark; do
            shot=$PWD/$out/$scene-$theme.png
            as_demo env -u WAYLAND_DISPLAY -u DBUS_SESSION_BUS_ADDRESS \
                DISPLAY=":$n" WINIT_X11_SCALE_FACTOR=$scale \
                WGPU_BACKEND=gl LIBGL_ALWAYS_SOFTWARE=1 \
                HOME=/home/demo SHELL=/bin/bash TZ=UTC LANG=C.UTF-8 \
                XDG_CONFIG_HOME="$work/xdg/config" XDG_DATA_HOME="$work/xdg/data" \
                XDG_CACHE_HOME="$work/xdg/cache" XDG_STATE_HOME="$work/xdg/state" \
                EGUI_FILES_SCENE="$scene" EGUI_FILES_THEME="$theme" \
                EGUI_FILES_SCREENSHOT="$shot" \
                "$app" >"$work/$scene-$theme.log" 2>&1 || {
                echo "screenshots.sh: $scene ($theme) failed; see $work/$scene-$theme.log" >&2
                exit 1
            }
            shrink "$shot"
            echo "$out/$scene-$theme.png"
        done
    done
}

# Runs a command with the demo's home at /home/demo, the other folders in
# /home still there: as root of a user namespace, a new /home is made in
# $stage and moved over the old, then the command runs as ourselves again, in
# a second namespace.
as_demo() {
    unshare --user --map-root-user --mount sh -c '
        stage=$1 home=$2 uid=$3 gid=$4
        shift 4
        mount -t tmpfs none "$stage" || exit
        for dir in /home/*; do
            [ -d "$dir" ] || continue
            mkdir "$stage/${dir##*/}" &&
                mount --rbind "$dir" "$stage/${dir##*/}" || exit
        done
        mkdir "$stage/demo" && mount --bind "$home" "$stage/demo" &&
            mount --move "$stage" /home || exit
        exec unshare --user --map-user="$uid" --map-group="$gid" -- "$@"
    ' sh "$stage" "$work/home" "$(id -u)" "$(id -g)" "$@"
}

shoot_web() {
    needs trunk python3 node firefox curl
    # A debug build: it looks the same, and builds far sooner.
    rm -rf target/screenshots/web target/screenshots/firefox
    CARGO_TARGET_DIR=target/wasm trunk build --dist "$work/web" web/demo/index.html \
        >"$work/trunk.log" 2>&1 || {
        echo "screenshots.sh: the web demo didn't build; see $work/trunk.log" >&2
        exit 1
    }
    web/demo-files.sh "$work/web/files"
    web/fonts.sh "$work/web/fonts" >/dev/null

    port=$((8700 + $$ % 200))
    python3 -m http.server "$port" --bind 127.0.0.1 -d "$work/web" \
        >"$work/http.log" 2>&1 &
    cleanup="$cleanup; kill $! 2>/dev/null || true"
    tries=0
    until curl -fs "http://127.0.0.1:$port/" >/dev/null; do
        tries=$((tries + 1))
        if [ "$tries" -gt 50 ]; then
            echo "screenshots.sh: the web server didn't start (see $work/http.log)" >&2
            exit 1
        fi
        sleep 0.1
    done
    for theme in light dark; do
        node scripts/web-screenshot.mjs "http://127.0.0.1:$port/" "$theme" \
            "web/screenshot-$theme.png" "$work/firefox/$theme"
        shrink "web/screenshot-$theme.png"
    done
}

[ -z "$desktop" ] || shoot_desktop
[ -z "$web" ] || shoot_web
