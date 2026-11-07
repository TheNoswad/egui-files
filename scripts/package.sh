#!/bin/sh
# Packages a release build for download, into target/dist/, named
# egui-files-<label>-<platform>.<ext>:
#
#   scripts/package.sh <platform> <label>
#
#   linux-x86_64, linux-aarch64   .AppImage, and .tar.gz with the bare binary
#   windows-x86_64, windows-aarch64   .zip
#   android-arm64, android-x86_64     .apk
#
# <label> is the version (0.1.0) or "nightly". The build comes first, for the
# platform's target (see .github/workflows/build.yml):
#   Linux and Windows   cargo build --release --target <triple>
#   Android             cargo apk build --release --target <triple>, in android/
#
# The AppImage tools are downloaded once, pinned and checked, into
# target/package-tools/. Windows on arm64 needs llvm-mingw on PATH, whose
# libunwind.dll the .exe needs beside it.
set -eu
cd "$(dirname "$0")/.."

[ $# -eq 2 ] || {
    echo "usage: scripts/package.sh <platform> <label>" >&2
    exit 2
}
platform=$1
label=$2
name=egui-files-$label-$platform
dist=target/dist
# Where a download is put together; afresh each time.
rm -rf target/package
mkdir -p "$dist" target/package

# What's in every desktop download beside the program.
docs="README.md LICENSE-MIT LICENSE-APACHE CHANGELOG.md"

# Fetches a pinned file into target/package-tools/, checking its SHA-256.
tool() {
    url=$1 sum=$2 file=target/package-tools/${1##*/}
    mkdir -p target/package-tools
    if ! echo "$sum  $file" | sha256sum -c --status 2>/dev/null; then
        curl -fsSL -o "$file.part" "$url"
        echo "$sum  $file.part" | sha256sum -c --quiet
        mv "$file.part" "$file"
    fi
    chmod +x "$file"
    echo "$file"
}

linux() {
    arch=$1
    bin=target/$arch-unknown-linux-gnu/release/egui-files
    [ -f "$bin" ] || {
        echo "package.sh: no $bin; build it first" >&2
        exit 1
    }

    # The bare binary, with what a desktop needs to list it.
    mkdir "target/package/$name"
    cp "$bin" packaging/linux/egui-files.desktop assets/icon/egui-files.png $docs \
        "target/package/$name/"
    tar -czf "$dist/$name.tar.gz" -C target/package "$name"
    echo "$dist/$name.tar.gz"

    # The AppImage: the same, laid out as an AppDir.
    appdir=target/package/$name.AppDir
    mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" \
        "$appdir/usr/share/icons/hicolor/512x512/apps"
    cp "$bin" "$appdir/usr/bin/"
    cp packaging/linux/egui-files.desktop "$appdir/usr/share/applications/"
    cp assets/icon/egui-files.png "$appdir/usr/share/icons/hicolor/512x512/apps/"
    cp packaging/linux/egui-files.desktop assets/icon/egui-files.png "$appdir/"
    ln -s egui-files.png "$appdir/.DirIcon"
    ln -s usr/bin/egui-files "$appdir/AppRun"

    appimagetool=$(tool https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage \
        ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0)
    case $arch in
        x86_64) sum=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d ;;
        aarch64) sum=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444 ;;
    esac
    runtime=$(tool "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-$arch" "$sum")
    # Extracted rather than mounted: CI machines have no FUSE.
    ARCH=$arch APPIMAGE_EXTRACT_AND_RUN=1 "$appimagetool" --no-appstream \
        --runtime-file "$runtime" "$appdir" "$dist/$name.AppImage" >target/package/appimagetool.log 2>&1 || {
        cat target/package/appimagetool.log >&2
        exit 1
    }
    echo "$dist/$name.AppImage"
}

windows() {
    triple=$1
    exe=target/$triple/release/egui-files.exe
    [ -f "$exe" ] || {
        echo "package.sh: no $exe; build it first" >&2
        exit 1
    }
    mkdir "target/package/$name"
    cp "$exe" $docs "target/package/$name/"
    if [ "$triple" = aarch64-pc-windows-gnullvm ]; then
        clang=$(command -v aarch64-w64-mingw32-clang) || {
            echo "package.sh: needs llvm-mingw on PATH, for libunwind.dll" >&2
            exit 1
        }
        cp "${clang%/bin/*}/aarch64-w64-mingw32/bin/libunwind.dll" "target/package/$name/"
    fi
    (cd target/package && zip -qr "$name.zip" "$name")
    mv "target/package/$name.zip" "$dist/"
    echo "$dist/$name.zip"
}

android() {
    apk=target/release/apk/egui-files.apk
    [ -f "$apk" ] || {
        echo "package.sh: no $apk; build it first" >&2
        exit 1
    }
    cp "$apk" "$dist/$name.apk"
    echo "$dist/$name.apk"
}

case $platform in
    linux-x86_64) linux x86_64 ;;
    linux-aarch64) linux aarch64 ;;
    windows-x86_64) windows x86_64-pc-windows-gnu ;;
    windows-aarch64) windows aarch64-pc-windows-gnullvm ;;
    android-arm64 | android-x86_64) android ;;
    *)
        echo "package.sh: no platform $platform" >&2
        exit 2
        ;;
esac
