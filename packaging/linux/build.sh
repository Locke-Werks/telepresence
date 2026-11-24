#!/usr/bin/env bash
# Build telepresence for Linux and package it as a .deb and an AppImage.
#
#   packaging/linux/build.sh [version]
#
# Targets Ubuntu 24.04 (noble), the oldest distro with the GStreamer 1.24
# headers the gstreamer-rs v1_24 feature needs. Runs as root in a bare
# ubuntu:24.04 container, or as a sudo-capable user on a noble host or runner.
# Set SKIP_DEPS=1 to skip the apt step when the dependencies are already there.
#
# Output lands in dist/linux/:
#   telepresence_<version>_amd64.deb
#   telepresence-<version>-linux-x86_64.AppImage
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HERE="$ROOT/packaging/linux"
DIST="$ROOT/dist/linux"
# Kept apart from the default target dir so a Windows or macOS build sharing
# the checkout is not overwritten.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/linux}"

VERSION="${1:-${VERSION:-}}"
if [ -z "$VERSION" ]; then
    VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p}' "$ROOT/Cargo.toml")"
fi
VERSION="${VERSION#v}"
[ -n "$VERSION" ] || { echo "could not determine version" >&2; exit 1; }
echo "==> telepresence $VERSION"

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"

if [ "${SKIP_DEPS:-0}" != "1" ]; then
    echo "==> apt dependencies"
    export DEBIAN_FRONTEND=noninteractive
    $SUDO apt-get update -q
    $SUDO apt-get install -yq --no-install-recommends \
        build-essential ca-certificates curl file git pkg-config patchelf \
        lld \
        desktop-file-utils appstream dpkg-dev \
        qmake6 qt6-base-dev qt6-base-dev-tools qt6-declarative-dev qt6-declarative-dev-tools \
        qt6-multimedia-dev qt6-wayland libgl-dev \
        qml6-module-qtquick qml6-module-qtquick-controls qml6-module-qtquick-layouts \
        qml6-module-qtquick-templates qml6-module-qtquick-window qml6-module-qtmultimedia \
        qml6-module-qtqml-workerscript qml6-module-qtquick-shapes \
        libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
        gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
        gstreamer1.0-plugins-ugly gstreamer1.0-libav gstreamer1.0-pipewire gstreamer1.0-vaapi \
        gstreamer1.0-tools gstreamer1.0-x
fi

if ! command -v cargo >/dev/null 2>&1; then
    [ -x "$HOME/.cargo/bin/cargo" ] || {
        echo "==> rustup"
        curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    }
    export PATH="$HOME/.cargo/bin:$PATH"
fi

echo "==> cargo build"
# cxx-qt finds Qt through qmake; noble ships the Qt 6 one as qmake6.
export QMAKE="${QMAKE:-$(command -v qmake6)}"
# qt-build-utils appends -fuse-ld=gold when ld is bfd and it cannot see lld,
# and gold resolves archives in one pass, so the cxx-qt initializer archives
# that follow libtp_app on the link line come up undefined. Naming the linker
# here makes qt-build-utils leave the choice alone.
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-fuse-ld=lld"
cargo build --release --locked -p tp-app --manifest-path "$ROOT/Cargo.toml"
BIN="$CARGO_TARGET_DIR/release/telepresence"
strip "$BIN"

desktop-file-validate "$HERE/telepresence.desktop"
# --no-net: the validator otherwise tries to fetch the homepage URL.
appstreamcli validate --no-net --pedantic "$HERE/com.lockewerks.TelePresence.metainfo.xml" || \
    appstreamcli validate --no-net "$HERE/com.lockewerks.TelePresence.metainfo.xml"

rm -rf "$DIST"
mkdir -p "$DIST"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Shared file layout for both packages.
stage() {
    local dest="$1"
    install -Dm755 "$BIN" "$dest/usr/bin/telepresence"
    install -Dm644 "$HERE/telepresence.desktop" "$dest/usr/share/applications/telepresence.desktop"
    install -Dm644 "$HERE/com.lockewerks.TelePresence.metainfo.xml" \
        "$dest/usr/share/metainfo/com.lockewerks.TelePresence.metainfo.xml"
    local size
    for size in 16 24 32 48 64 128 256 512; do
        mkdir -p "$dest/usr/share/icons/hicolor/${size}x${size}/apps"
        convert_icon "$ROOT/assets/telepresence.png" "$dest/usr/share/icons/hicolor/${size}x${size}/apps/telepresence.png" "$size"
    done
}

# The source icon is 512px; smaller hicolor sizes are scaled with Qt itself
# (already a build dependency) so no image tool has to be installed.
convert_icon() {
    if [ "$3" = 512 ]; then cp "$1" "$2"; return; fi
    "$WORK/iconscale" "$1" "$2" "$3"
}
build_iconscale() {
    cat > "$WORK/iconscale.cpp" <<'CPP'
#include <QGuiApplication>
#include <QImage>
int main(int argc, char **argv) {
    QImage src(argv[1]);
    int s = atoi(argv[3]);
    return src.scaled(s, s, Qt::KeepAspectRatio, Qt::SmoothTransformation).save(argv[2]) ? 0 : 1;
}
CPP
    g++ -fPIC -O2 -o "$WORK/iconscale" "$WORK/iconscale.cpp" \
        $(pkg-config --cflags --libs Qt6Gui)
}
build_iconscale

echo "==> .deb"
PKG="$WORK/deb"
stage "$PKG"
install -Dm644 /dev/stdin "$PKG/usr/share/doc/telepresence/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: telepresence
Source: https://github.com/Locke-Werks/telepresence

Files: *
Copyright: Locke Werks
License: GPL-3.0-only
 On Debian systems the full text is in /usr/share/common-licenses/GPL-3.
EOF

# Shared-library deps come from dpkg-shlibdeps so they track whatever the
# binary actually links; it wants a debian/control to run against.
mkdir -p "$WORK/shl/debian"
printf 'Source: telepresence\n\nPackage: telepresence\nArchitecture: amd64\n' > "$WORK/shl/debian/control"
SHLIBS="$(cd "$WORK/shl" && dpkg-shlibdeps -O "$PKG/usr/bin/telepresence" 2>/dev/null | sed -n 's/^shlibs:Depends=//p')"
# Plugins and QML modules are loaded at runtime, so no linker check sees them.
# The shlibs floor for libgstreamer is 1.20; the v1_24 bindings call 1.24 API.
RUNTIME="libgstreamer1.0-0 (>= 1.24), gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-plugins-bad, \
gstreamer1.0-plugins-ugly, gstreamer1.0-libav, gstreamer1.0-pipewire, \
qml6-module-qtquick, qml6-module-qtquick-controls, qml6-module-qtquick-layouts, \
qml6-module-qtquick-templates, qml6-module-qtquick-window, qml6-module-qtmultimedia, \
qml6-module-qtqml-workerscript, qt6-qpa-plugins"
INSTALLED_KB="$(du -sk "$PKG/usr" | cut -f1)"

mkdir -p "$PKG/DEBIAN"
cat > "$PKG/DEBIAN/control" <<EOF
Package: telepresence
Version: $VERSION
Architecture: amd64
Maintainer: Locke Werks <archon@lockewerks.com>
Installed-Size: $INSTALLED_KB
Depends: ${SHLIBS:+$SHLIBS, }$RUNTIME
Recommends: gstreamer1.0-vaapi, qt6-wayland
Section: video
Priority: optional
Homepage: https://github.com/Locke-Werks/telepresence
Description: Camera and microphone over the LAN
 Streams a camera and microphone from one machine to another on the local
 network. On Linux it sends; receiving shows a preview only.
EOF
DEB="$DIST/telepresence_${VERSION}_amd64.deb"
dpkg-deb --root-owner-group --build "$PKG" "$DEB"

echo "==> AppImage"
TOOLS="$WORK/tools"
mkdir -p "$TOOLS"
fetch() { curl -fsSL --retry 3 -o "$TOOLS/$1" "$2"; chmod +x "$TOOLS/$1"; }
fetch linuxdeploy-x86_64.AppImage \
    https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
fetch linuxdeploy-plugin-qt-x86_64.AppImage \
    https://github.com/linuxdeploy/linuxdeploy-plugin-qt/releases/download/continuous/linuxdeploy-plugin-qt-x86_64.AppImage
fetch linuxdeploy-plugin-gstreamer.sh \
    https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gstreamer/master/linuxdeploy-plugin-gstreamer.sh

APPDIR="$WORK/AppDir"
stage "$APPDIR"
# Containers and CI runners have no FUSE; the tools unpack themselves instead.
export APPIMAGE_EXTRACT_AND_RUN=1
export PATH="$TOOLS:$PATH"
export QML_SOURCES_PATHS="$ROOT/crates/tp-app/qml"
# xcb is the plugin's default; wayland for native sessions, offscreen for
# headless smoke tests.
export EXTRA_PLATFORM_PLUGINS="libqwayland-egl.so;libqwayland-generic.so;libqoffscreen.so"
export LINUXDEPLOY_OUTPUT_VERSION="$VERSION"
APPIMAGE="$DIST/telepresence-${VERSION}-linux-x86_64.AppImage"
export LDAI_OUTPUT="$APPIMAGE"
export OUTPUT="$APPIMAGE"
(
    cd "$WORK"
    linuxdeploy-x86_64.AppImage --appdir "$APPDIR" \
        --executable "$APPDIR/usr/bin/telepresence" \
        --desktop-file "$APPDIR/usr/share/applications/telepresence.desktop" \
        --icon-file "$ROOT/assets/telepresence.png" \
        --plugin qt --plugin gstreamer \
        `# On linuxdeploy's excludelist, but gst-libav (avdec_h264, the software` \
        `# decoder) fails to load without them on a minimal host.` \
        --library "/usr/lib/$(uname -m)-linux-gnu/libfribidi.so.0" \
        --library "/usr/lib/$(uname -m)-linux-gnu/libusb-1.0.so.0" \
        --output appimage
)
chmod +x "$APPIMAGE"

echo "==> done"
ls -l "$DIST"
