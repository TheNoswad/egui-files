#!/bin/sh
# Prints the notes of a nightly built from the commit checked out: what's new
# since the commit the last one was built from, which is given. It needs the
# history. With nothing to compare with (no last nightly, or it isn't in the
# history after a force-push; give ""), there's no list.
set -eu
cd "$(dirname "$0")/.."
[ $# -eq 1 ] || {
    echo "usage: scripts/nightly-notes.sh <commit of the last nightly, or \"\">" >&2
    exit 2
}
previous=$1
head=$(git rev-parse HEAD)
repo=${GITHUB_REPOSITORY:-$(git remote get-url origin | sed -E 's#.*github\.com[:/]##; s#\.git$##')}
url=${GITHUB_SERVER_URL:-https://github.com}/$repo

echo "Built from main at $(git rev-parse --short "$head"), replaced on every push. For"
echo "trying what's new; it may break. Releases are steadier."
if [ -n "$previous" ] && git merge-base --is-ancestor "$previous" "$head" 2>/dev/null; then
    commits=$(git log --format="- %s ([%h]($url/commit/%H))" "$previous..$head")
    if [ -n "$commits" ]; then
        printf '\n### Since the last nightly\n\n%s\n' "$commits"
    fi
fi
printf '\nAll of it: [CHANGELOG.md](%s/blob/%s/CHANGELOG.md).\n' "$url" "$head"
