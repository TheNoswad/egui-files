#!/bin/sh
# Puts the files the demo fetches when they're first opened in a folder beside
# its page, with a manifest listing them: whatever's in web/demo-files/, by the
# path it should have under the demo's home folder (say Pictures/beach.jpg).
#   web/demo-files.sh <out-dir>
set -e
cd "$(dirname "$0")/.."
out=$1
mkdir -p "$out"
if [ -d web/demo-files ]; then
    cp -R web/demo-files/. "$out/"
fi
manifest=manifest.txt
cd "$out"
# size, modified (seconds since 1970), path; tabs between. A name can't contain
# a newline or tab here.
find . -type f ! -name "$manifest" ! -name "$manifest.new" | sed 's|^\./||' | sort | while IFS= read -r file; do
    printf '%s\t%s\t%s\n' "$(wc -c <"$file" | tr -d ' ')" "$(date -r "$file" +%s)" "$file"
done >"$manifest.new"
mv "$manifest.new" "$manifest"
