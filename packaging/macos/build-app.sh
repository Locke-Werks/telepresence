#!/usr/bin/env bash
# Build TelePresence.app: release binary (one arch or universal), Info.plist,
# icon, Qt via macdeployqt, and a private copy of GStreamer. Unsigned; run
# sign-notarize.sh afterwards.
#
# Environment (all optional):
#   VERSION    bundle version              default: workspace version in Cargo.toml
#   ARCHS      "arm64", "x86_64" or both   default: host arch
#   QMAKE      qmake of the Qt to deploy   default: qmake6 or qmake on PATH
#   GST_ROOT   official GStreamer.framework root
#              default: /Library/Frameworks/GStreamer.framework/Versions/1.0
#   OUT_DIR    where the .app lands        default: target/macos
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

VERSION="${VERSION:-$(sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' Cargo.toml)}"
ARCHS="${ARCHS:-$(uname -m)}"
GST_ROOT="${GST_ROOT:-/Library/Frameworks/GStreamer.framework/Versions/1.0}"
OUT_DIR="${OUT_DIR:-$ROOT/target/macos}"
QMAKE="${QMAKE:-$(command -v qmake6 || command -v qmake)}"
QT_BIN="$("$QMAKE" -query QT_INSTALL_BINS)"
APP="$OUT_DIR/TelePresence.app"

# Every element tp-media can ask for on macOS, by plugin file. Adding an
# element to the pipeline code without adding its plugin here fails at runtime,
# not at build time.
GST_PLUGINS=(
  coreelements      # queue, tee, capsfilter
  app               # appsrc, appsink
  autodetect        # autovideosrc/sink, autoaudiosrc/sink
  videoconvertscale # videoconvert, videoscale
  videofilter       # videoflip
  videotestsrc
  audioconvert
  audioresample
  audiotestsrc
  opus              # opusenc, opusdec
  applemedia        # avfvideosrc, vtenc_h264, vtdec, AVF device provider
  osxaudio          # osxaudiosrc/sink, CoreAudio device provider
  osxvideo          # osxvideosink, what autovideosink picks
  videoparsersbad   # h264parse
  libav             # avdec_h264, software decode fallback
  x264              # x264enc, software encode fallback
  jpeg              # jpegdec for MJPEG cameras
)

[ -d "$GST_ROOT/lib/gstreamer-1.0" ] || { echo "GStreamer.framework not found at $GST_ROOT" >&2; exit 1; }
[ -x "$QT_BIN/macdeployqt" ] || { echo "macdeployqt not found in $QT_BIN" >&2; exit 1; }
echo "TelePresence $VERSION for $ARCHS, Qt $("$QMAKE" -query QT_VERSION), GStreamer at $GST_ROOT"

# Link against the framework's .pc files only. PKG_CONFIG_LIBDIR (not _PATH)
# keeps Homebrew's GStreamer out entirely, so the binary cannot end up linked
# to a library the bundle does not carry.
export PKG_CONFIG_LIBDIR="$GST_ROOT/lib/pkgconfig"
export PKG_CONFIG_ALLOW_CROSS=1
export QMAKE
export MACOSX_DEPLOYMENT_TARGET=13.0
# Room for install_name_tool to rewrite load commands to @rpath.
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-headerpad_max_install_names"

BINS=()
for arch in $ARCHS; do
  case "$arch" in
    arm64)  triple=aarch64-apple-darwin ;;
    x86_64) triple=x86_64-apple-darwin ;;
    *) echo "unknown arch $arch" >&2; exit 1 ;;
  esac
  cargo build --release --locked -p tp-app --bin telepresence --target "$triple"
  BINS+=("target/$triple/release/telepresence")
done

# cxx-qt adds Qt's lib dir to the link search path. When that is a shared
# prefix (Homebrew's /opt/homebrew/lib) holding another GStreamer, ld can pick
# that one over the framework, and the bundle then mixes two GStreamer
# versions. Compare the linked compatibility version against the framework.
gst_compat() { otool -L "$1" | awk '/libgstreamer-1\.0\.0\.dylib \(compatibility/ { print $4; exit }' | tr -d ,; }
want="$(gst_compat "$GST_ROOT/lib/libgstreamer-1.0.0.dylib")"
for bin in "${BINS[@]}"; do
  got="$(gst_compat "$bin")"
  if [ "$got" != "$want" ]; then
    echo "$bin links libgstreamer compatibility $got, framework is $want." >&2
    echo "Another GStreamer is on the link path (likely the Qt prefix); use a Qt outside it." >&2
    exit 1
  fi
done

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
if [ "${#BINS[@]}" -gt 1 ]; then
  lipo -create "${BINS[@]}" -output "$APP/Contents/MacOS/telepresence"
else
  cp "${BINS[0]}" "$APP/Contents/MacOS/telepresence"
fi
sed "s/@VERSION@/$VERSION/g" "$HERE/Info.plist.in" > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist" >/dev/null
printf 'APPL????' > "$APP/Contents/PkgInfo"
cp assets/telepresence.icns "$APP/Contents/Resources/telepresence.icns"

# Qt first: it owns Contents/Frameworks/*.framework and Contents/PlugIns/<qt>.
# No -sign here; sign-notarize.sh signs everything in one pass afterwards.
"$QT_BIN/macdeployqt" "$APP" -qmldir="$ROOT/crates/tp-app/qml" -verbose=1

# macdeployqt deploys every SQL driver Qt Sql has. Only SQLite (QML
# LocalStorage) is self-contained; the rest link client libraries (libpq,
# iODBC, Mimer) that are not on the machine and that nothing here uses.
find "$APP/Contents/PlugIns/sqldrivers" -name '*.dylib' ! -name 'libqsqlite.dylib' -delete 2>/dev/null || true

QT_INSTALL_LIBS="$("$QMAKE" -query QT_INSTALL_LIBS)" \
  python3 "$HERE/bundle_deps.py" "$APP" "$GST_ROOT" "${GST_PLUGINS[@]}"

lipo -info "$APP/Contents/MacOS/telepresence"
du -sh "$APP"
echo "$APP"
