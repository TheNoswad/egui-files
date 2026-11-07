#!/bin/sh
# Keeps the patched crate copies in this folder honest: each must be the
# published crate (patches/crates says which version, and what it leaves
# out) plus patches/<crate>.diff, and nothing else.
#
#   patches/patches.sh check          every copy matches its crate + diff
#   patches/patches.sh record [crate] rewrite the diff from the copy, after
#                                     changing the copy
#   patches/patches.sh apply <crate>  make the copy from the crate + diff
#                                     (the copy mustn't exist yet)
#
# To move a patch to a new version: `git rm -r patches/<crate>`, change the
# version in patches/crates, `patches.sh apply <crate>`, fix what didn't
# apply, `patches.sh record <crate>`.
#
# The published crates are downloaded from crates.io into target/patches,
# checked against the checksum in crates.io's index (the one Cargo checks).
set -eu
cd "$(dirname "$0")/.."

cache=target/patches
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
# out, into $cache/base/<crate>-<version>; prints that folder.
base() {
    crate=$1
    set -- $(entry "$crate")
    version=$1
    shift
    dir=$cache/base/$crate-$version
    if [ ! -d "$dir" ]; then
        mkdir -p "$cache/crates" "$cache/base"
        file=$cache/crates/$crate-$version.crate
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
        mkdir "$dir.part"
        # shellcheck disable=SC2086
        tar -xzf "$file" -C "$dir.part" --strip-components=1 --anchored $excludes "$crate-$version"
        mv "$dir.part" "$dir"
    fi
    echo "$dir"
}

# The difference between the published crate and the copy, as a patch for
# `patch -p1` (paths a/… and b/…, no timestamps, so it's the same each run).
make_diff() {
    crate=$1
    dir=$(base "$crate")
    [ -d "patches/$crate" ] || die "patches/$crate doesn't exist"
    # diff exits with 1 when the trees differ, which they do.
    LC_ALL=C diff -ruN "$dir" "patches/$crate" | sed \
        -e "/^diff -ruN /d" \
        -e "s|^--- $dir/\([^	]*\).*|--- a/\1|" \
        -e "s|^+++ patches/$crate/\([^	]*\).*|+++ b/\1|" || true
}

check() {
    failed=
    for crate in $(crates); do
        [ -f "patches/$crate.diff" ] || die "patches/$crate.diff doesn't exist"
        if make_diff "$crate" | cmp -s - "patches/$crate.diff"; then
            echo "patches.sh: $crate matches its crate and diff" >&2
        else
            echo "patches.sh: patches/$crate differs from its crate + $crate.diff:" >&2
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
        make_diff "$crate" >"patches/$crate.diff"
        echo "patches.sh: wrote patches/$crate.diff ($(grep -c '^+++ ' "patches/$crate.diff") files)" >&2
    done
}

apply() {
    crate=$1
    [ ! -e "patches/$crate" ] || die "patches/$crate already exists; git rm -r it first"
    dir=$(base "$crate")
    cp -R "$dir" "patches/$crate"
    if patch -p1 --no-backup-if-mismatch -d "patches/$crate" <"patches/$crate.diff" >&2; then
        echo "patches.sh: made patches/$crate" >&2
    else
        die "the diff didn't all apply; fix patches/$crate (see the .rej files), then: patches/patches.sh record $crate"
    fi
}

case ${1:-} in
    check) check ;;
    record) shift; record "$@" ;;
    apply) [ $# -eq 2 ] || die "usage: patches.sh apply <crate>"; apply "$2" ;;
    *) die "usage: patches.sh check | record [crate] | apply <crate>" ;;
esac
