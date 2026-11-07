#!/bin/sh
# Makes the patched crates Cargo.toml's [patch.crates-io] points at: each is
# the published crate (patches/crates says which version, and what it
# leaves out) plus patches/<crate>.diff. They're made in patches/build
# (not in git), and patches/build/<crate> links to the one made last.
#
#   patches/patches.sh ensure         make what's missing or out of date; run
#                                     once after cloning (builds need it), and
#                                     by the pre-commit hook and web/build.sh
#   patches/patches.sh check          each copy is still its crate + diff
#   patches/patches.sh record [crate] rewrite the diff from the copy, after
#                                     changing the copy (patches/build/<crate>)
#
# To move a patch to a new version: change its version in patches/crates and
# run ensure. If the diff doesn't all apply, the copy is left as far as it
# got, with .rej files saying what's left: fix it, then record.
#
# The published crates are downloaded from crates.io, checked against the
# checksum in crates.io's index (the one Cargo checks). Nothing here is ever
# deleted: an out-of-date copy is left, and the link moves to a new one.
set -eu
cd "$(dirname "$0")/.."

build=patches/build
always_left_out=".cargo_vcs_info.json Cargo.toml.orig Cargo.lock"

die() {
    echo "patches.sh: $*" >&2
    exit 1
}

# "version left-out..." for a crate in patches/crates.
entry() {
    line=$(grep -v '^#' patches/crates | awk -v c="$1" '$1 == c { $1 = ""; print }')
    [ -n "$line" ] || die "$1 isn't in patches/crates"
    echo $line
}

crates() {
    grep -v '^#' patches/crates | awk 'NF { print $1 }'
}

# The crate's path in crates.io's sparse index.
index_path() {
    name=$(printf %s "$1" | tr '[:upper:]' '[:lower:]')
    case ${#name} in
        1) echo "1/$name" ;;
        2) echo "2/$name" ;;
        3) echo "3/$(printf %s "$name" | cut -c1)/$name" ;;
        *) echo "$(printf %s "$name" | cut -c1-2)/$(printf %s "$name" | cut -c3-4)/$name" ;;
    esac
}

# Downloads and unpacks the published crate, without what the copy leaves
# out, into $build/base/<crate>-<version>; prints that folder.
base() {
    crate=$1
    set -- $(entry "$crate")
    version=$1
    shift
    dir=$build/base/$crate-$version
    if [ ! -d "$dir" ]; then
        mkdir -p "$build/crates" "$build/base"
        file=$build/crates/$crate-$version.crate
        if [ ! -f "$file" ]; then
            sum=$(curl -fsSL "https://index.crates.io/$(index_path "$crate")" |
                grep "\"vers\":\"$version\"" |
                sed 's/.*"cksum":"\([0-9a-f]*\)".*/\1/')
            [ -n "$sum" ] || die "crates.io's index has no $crate $version"
            curl -fsSL -o "$file.part" "https://static.crates.io/crates/$crate/$crate-$version.crate"
            echo "$sum  $file.part" | sha256sum -c --quiet - >&2 ||
                die "$crate-$version.crate doesn't match crates.io's checksum"
            mv "$file.part" "$file"
        fi
        excludes=
        for path in $always_left_out "$@"; do
            excludes="$excludes --exclude=$crate-$version/$path"
        done
        part=$(mktemp -d "$build/base/.part.XXXXXX")
        chmod 755 "$part"
        # shellcheck disable=SC2086
        tar -xzf "$file" -C "$part" --strip-components=1 --anchored $excludes "$crate-$version"
        mv "$part" "$dir"
    fi
    echo "$dir"
}

# The folder a crate's copy is made in: named after its version and what
# goes into it, so a change to either makes a new one.
copy_name() {
    crate=$1
    set -- $(entry "$crate")
    hash=$( { echo "$*"; cat "patches/$crate.diff"; } | sha256sum | cut -c1-12)
    echo "$crate-$1-$hash"
}

ensure() {
    mkdir -p "$build"
    failed=
    for crate in $(crates); do
        [ -f "patches/$crate.diff" ] || die "patches/$crate.diff doesn't exist"
        link=$build/$crate
        name=$(copy_name "$crate")
        current=$(readlink "$link" 2>/dev/null || true)
        # A copy left half-patched is the user's to finish, while it's still
        # the version and diff wanted; recording it changes the diff.
        case $current in
            ".part.$name."*)
                echo "patches.sh: $link is being moved to a new version ($current);" \
                    "finish it, then: patches/patches.sh record $crate" >&2
                failed=1
                continue
                ;;
        esac
        # The copy linked to, if it's for this version and diff, is left as it
        # is, even with changes not recorded yet: check says so.
        case $current in
            "$name" | "$name".[0-9]*) continue ;;
        esac
        # Otherwise one made before that's still as it was made, or a new one.
        n=0
        target=$name
        while [ -d "$build/$target" ] &&
            ! make_diff "$crate" "$build/$target" | cmp -s - "patches/$crate.diff"; do
            n=$((n + 1))
            target=$name.$n
        done
        if [ ! -d "$build/$target" ]; then
            dir=$(base "$crate")
            part=$(mktemp -d "$build/.part.$name.XXXXXX")
            chmod 755 "$part"
            cp -R "$dir/." "$part"
            if ! patch -s -p1 --no-backup-if-mismatch -d "$part" <"patches/$crate.diff" >&2; then
                ln -sfn "${part#"$build"/}" "$link"
                echo "patches.sh: patches/$crate.diff didn't all apply to $(entry "$crate" | cut -d' ' -f1);" \
                    "fix $link (its .rej files say what's left), then: patches/patches.sh record $crate" >&2
                failed=1
                continue
            fi
            mv "$part" "$build/$target"
        fi
        ln -sfn "$target" "$link"
        echo "patches.sh: $link is now $target" >&2
    done
    [ -z "$failed" ]
}

# The difference between the published crate and the copy (or the folder
# given), as a patch for `patch -p1` (paths a/… and b/…, no timestamps, so it's the same each run).
make_diff() {
    crate=$1
    dir=$(base "$crate")
    copy=${2:-$build/$crate}
    [ -d "$copy" ] || die "$copy doesn't exist; run patches/patches.sh ensure"
    # diff exits with 1 when the trees differ, which they do. Followed with a
    # slash, as the copy is a link.
    LC_ALL=C diff -ruN -x '*.rej' -x '*.orig' "$dir" "$copy/" | sed \
        -e "/^diff -ruN /d" \
        -e "s|^--- $dir/\([^	]*\).*|--- a/\1|" \
        -e "s|^+++ $copy//*\([^	]*\).*|+++ b/\1|" || true
}

check() {
    failed=
    for crate in $(crates); do
        [ -f "patches/$crate.diff" ] || die "patches/$crate.diff doesn't exist"
        if make_diff "$crate" | cmp -s - "patches/$crate.diff"; then
            echo "patches.sh: $crate matches its crate and diff" >&2
        else
            echo "patches.sh: $build/$crate differs from its crate + $crate.diff:" >&2
            make_diff "$crate" | diff -u "patches/$crate.diff" - | head -40 >&2 || true
            echo "  (patches/patches.sh record $crate, if the copy is right)" >&2
            failed=1
        fi
    done
    [ -z "$failed" ]
}

record() {
    [ $# -gt 0 ] || set -- $(crates)
    for crate in "$@"; do
        entry "$crate" >/dev/null
        rejects=$(find -L "$build/$crate/" -name '*.rej' | head -1)
        [ -z "$rejects" ] || die "$rejects is still there: finish fixing $build/$crate first"
        make_diff "$crate" >"patches/$crate.diff.part"
        mv "patches/$crate.diff.part" "patches/$crate.diff"
        echo "patches.sh: wrote patches/$crate.diff ($(grep -c '^+++ ' "patches/$crate.diff") files)" >&2
    done
    # The copies are named after their diffs: link each to its new name.
    ensure
}

case ${1:-} in
    ensure) ensure ;;
    check) check ;;
    record) shift; record "$@" ;;
    *) die "usage: patches.sh ensure | check | record [crate]" ;;
esac
