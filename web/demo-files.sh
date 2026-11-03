#!/bin/sh
# Puts the demo's files beside its page, with a manifest listing them: all of
# web/demo-files/, by the path each has under the demo's home folder.
#   web/demo-files.sh <out-dir>
set -e
cd "$(dirname "$0")/.."
out=$1
mkdir -p "$out"
cp -RP web/demo-files/. "$out/"
manifest=manifest.txt
cd "$out"
# A file: size, age in seconds, path. A link: l, target, path. With tabs
# between, so names can't have tabs or newlines.
#
# Ages rather than dates: a checkout dates everything alike, and the demo's
# dates should look lived in whenever it's visited. Each file's comes from its
# name, up to 45 days, so it stays put between builds.
find . \( -type f -o -type l \) ! -name "$manifest" ! -name "$manifest.new" |
    sed 's|^\./||' | sort | while IFS= read -r file; do
    if [ -L "$file" ]; then
        printf 'l\t%s\t%s\n' "$(readlink "$file")" "$file"
    else
        sum=$(printf '%s' "$file" | cksum | cut -d' ' -f1)
        age=$((sum % (45 * 86400)))
        printf '%s\t%s\t%s\n' "$(wc -c <"$file" | tr -d ' ')" "$age" "$file"
    fi
done >"$manifest.new"
mv "$manifest.new" "$manifest"
# The manifest has the links, and some hosts refuse a site with any in it.
find . -type l -delete
