#!/bin/sh
# Prints a section of CHANGELOG.md, without its heading: a version's
# (scripts/release-notes.sh 0.1.0), or Unreleased.
set -eu
cd "$(dirname "$0")/.."
[ $# -eq 1 ] || {
    echo "usage: scripts/release-notes.sh <version>|Unreleased" >&2
    exit 2
}
notes=$(awk -v want="$1" '
    /^## / {
        # "## 0.1.0 (2026-11-07)" or "## Unreleased"
        name = $2
        if (in_it) exit
        in_it = (name == want)
        next
    }
    in_it
' CHANGELOG.md)
if [ -z "$(printf '%s' "$notes" | tr -d '[:space:]')" ]; then
    echo "release-notes.sh: CHANGELOG.md has nothing under \"## $1\"" >&2
    exit 1
fi
printf '%s\n' "$notes" | sed '/./,$!d'
