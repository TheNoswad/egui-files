#!/bin/sh
# What a build's downloads are labeled with (see scripts/package.sh): the
# version for a v* tag in CI, which must be Cargo.toml's; "nightly" for main
# in CI; "dev" anywhere else.
set -eu
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
case ${GITHUB_REF:-} in
    refs/tags/v*)
        tag=${GITHUB_REF#refs/tags/v}
        if [ "$tag" != "$version" ]; then
            echo "label.sh: tag v$tag, but Cargo.toml says $version" >&2
            exit 1
        fi
        echo "$version"
        ;;
    refs/heads/main) echo nightly ;;
    *) echo dev ;;
esac
