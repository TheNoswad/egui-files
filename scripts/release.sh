#!/bin/sh
# Makes a release commit and tag, ready to push:
#
#   scripts/release.sh 0.2.0
#   git push origin main v0.2.0     CI builds it and makes the GitHub release
#
# Sets the version in Cargo.toml and android/Cargo.toml (Android's
# version_code comes from it, and must go up), and dates CHANGELOG.md's
# Unreleased section as this version, under a new empty Unreleased.
#
# Versions are 0.MINOR.PATCH until 1.0: MINOR for a release with new
# features, PATCH for one with only fixes.
set -eu
cd "$(dirname "$0")/.."

[ $# -eq 1 ] || {
    echo "usage: scripts/release.sh <version>    (e.g. 0.2.0)" >&2
    exit 2
}
version=$1
echo "$version" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+' || {
    echo "release.sh: $version isn't MAJOR.MINOR.PATCH" >&2
    exit 1
}
[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || {
    echo "release.sh: releases are made from main" >&2
    exit 1
}
[ -z "$(git status --porcelain)" ] || {
    echo "release.sh: the working tree has changes; commit or stash them" >&2
    exit 1
}
if git rev-parse -q --verify "refs/tags/v$version" >/dev/null; then
    echo "release.sh: v$version is already tagged" >&2
    exit 1
fi
scripts/release-notes.sh Unreleased >/dev/null

# The package's own version: the first one in each file.
for manifest in Cargo.toml android/Cargo.toml; do
    sed -i "0,/^version = \".*\"$/s//version = \"$version\"/" "$manifest"
done
cargo update --quiet --offline --workspace
sed -i "0,/^## Unreleased$/s//## Unreleased\n\n## $version ($(date -u +%F))/" CHANGELOG.md

git add Cargo.toml android/Cargo.toml Cargo.lock CHANGELOG.md
git commit -q -m "Release $version"
git tag -a "v$version" -m "egui-files $version"
echo "Tagged v$version. To publish: git push origin main v$version"
