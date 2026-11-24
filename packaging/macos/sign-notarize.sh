#!/usr/bin/env bash
# Sign TelePresence.app inside-out, notarize and staple it, then produce the
# release pair:
#   telepresence-<version>-macos-<arch>.dmg   for people (Applications link)
#   telepresence-<version>-macos-<arch>.zip   for the updater
# <arch> is arm64, x86_64 or universal, read from the binary.
#
# Environment:
#   APP             default target/macos/TelePresence.app
#   DIST_DIR        default target/macos/dist
#   VERSION         default CFBundleShortVersionString of the app
#   SIGN_IDENTITY   default "Developer ID Application: Joseph Hancuff (KVJL9RB8BY)"
#   KEYCHAIN        keychain holding the identity (optional)
# Notarization credentials, one of:
#   NOTARY_PROFILE [+ NOTARY_KEYCHAIN]          stored notarytool profile
#   ASC_KEY_PATH + ASC_KEY_ID + ASC_ISSUER_ID   App Store Connect API key
#   SKIP_NOTARIZE=1                             sign only, for a local dry run
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
APP="${APP:-$ROOT/target/macos/TelePresence.app}"
DIST_DIR="${DIST_DIR:-$ROOT/target/macos/dist}"
SIGN_IDENTITY="${SIGN_IDENTITY:-Developer ID Application: Joseph Hancuff (KVJL9RB8BY)}"
ENTITLEMENTS="$HERE/entitlements.plist"
VERSION="${VERSION:-$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")}"

# file, not lipo: on macOS 27 lipo is SIGKILLed when pointed into a bundle
# whose signatures install_name_tool has invalidated, which is exactly the
# state the app is in before this script signs it.
archs="$(file -b "$APP/Contents/MacOS/telepresence")"
case "$archs" in
  *universal*x86_64*arm64*|*universal*arm64*x86_64*) ARCH=universal ;;
  *arm64*) ARCH=arm64 ;;
  *x86_64*) ARCH=x86_64 ;;
  *) echo "unexpected binary: $archs" >&2; exit 1 ;;
esac
BASE="telepresence-$VERSION-macos-$ARCH"
mkdir -p "$DIST_DIR"

SIGN=(codesign --force --sign "$SIGN_IDENTITY" --options runtime --timestamp)
[ -n "${KEYCHAIN:-}" ] && SIGN+=(--keychain "$KEYCHAIN")

is_macho() {
  case "$(file -b "$1")" in *Mach-O*) return 0 ;; *) return 1 ;; esac
}

# --- sign, deepest first ------------------------------------------------------
# --deep is not used: it signs nested code with the outer bundle's options and
# entitlements, and Apple documents it as unsuitable for signing.
echo "Signing loose Mach-O files"
main="$APP/Contents/MacOS/telepresence"
# Depth first so a dylib is signed before anything that seals it. Bundle paths
# contain no newlines, so line-based sorting is safe here.
find "$APP/Contents" -type f ! -path '*.framework/*' ! -path "$main" |
  awk -F/ '{ print NF "\t" $0 }' | sort -rn | cut -f2- |
  while IFS= read -r f; do
    is_macho "$f" || continue
    "${SIGN[@]}" "$f"
  done

echo "Signing frameworks"
find "$APP/Contents/Frameworks" -maxdepth 1 -name '*.framework' -type d -print0 |
  while IFS= read -r -d '' fw; do
    "${SIGN[@]}" "$fw"
  done

echo "Signing the app"
"${SIGN[@]}" --entitlements "$ENTITLEMENTS" "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"
codesign -d --entitlements - --xml "$APP" >/dev/null

# --- notarize ----------------------------------------------------------------
notarize() {
  local file="$1" out id status
  local cred=()
  if [ -n "${NOTARY_PROFILE:-}" ]; then
    cred=(--keychain-profile "$NOTARY_PROFILE")
    [ -n "${NOTARY_KEYCHAIN:-}" ] && cred+=(--keychain "$NOTARY_KEYCHAIN")
  else
    cred=(--key "${ASC_KEY_PATH:?}" --key-id "${ASC_KEY_ID:?}" --issuer "${ASC_ISSUER_ID:?}")
  fi
  echo "Notarizing $(basename "$file")"
  out="$(xcrun notarytool submit "$file" "${cred[@]}" --wait --timeout 30m --output-format json)"
  id="$(plutil -extract id raw -o - - <<<"$out")"
  status="$(plutil -extract status raw -o - - <<<"$out")"
  echo "  submission $id: $status"
  if [ "$status" != "Accepted" ]; then
    xcrun notarytool log "$id" "${cred[@]}" >&2 || true
    return 1
  fi
}

if [ "${SKIP_NOTARIZE:-}" = 1 ]; then
  echo "SKIP_NOTARIZE=1: not notarizing; Gatekeeper will reject this build"
else
  # The zip only carries the app to notarytool; the ticket is stapled to the app.
  upload="$(mktemp -d)/upload.zip"
  ditto -c -k --sequesterRsrc --keepParent "$APP" "$upload"
  notarize "$upload"
  rm -f "$upload"
  xcrun stapler staple "$APP"
  xcrun stapler validate "$APP"
  verdict="$(spctl -a -vvv -t exec "$APP" 2>&1)"
  echo "$verdict"
  grep -q 'source=Notarized Developer ID' <<<"$verdict"
fi

# --- updater zip --------------------------------------------------------------
# ditto keeps symlinks, xattrs and the stapled ticket; plain zip breaks the
# framework symlinks and with them the signature.
rm -f "$DIST_DIR/$BASE.zip"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$DIST_DIR/$BASE.zip"

# --- dmg ---------------------------------------------------------------------
stage="$(mktemp -d)"
ditto "$APP" "$stage/TelePresence.app"
ln -s /Applications "$stage/Applications"
rm -f "$DIST_DIR/$BASE.dmg"
hdiutil create -volname "TelePresence" -srcfolder "$stage" -ov -format UDZO \
  -fs HFS+ "$DIST_DIR/$BASE.dmg" >/dev/null
rm -rf "$stage"
dmg_sign=(codesign --force --sign "$SIGN_IDENTITY" --timestamp)
[ -n "${KEYCHAIN:-}" ] && dmg_sign+=(--keychain "$KEYCHAIN")
"${dmg_sign[@]}" "$DIST_DIR/$BASE.dmg"
if [ "${SKIP_NOTARIZE:-}" != 1 ]; then
  notarize "$DIST_DIR/$BASE.dmg"
  xcrun stapler staple "$DIST_DIR/$BASE.dmg"
  xcrun stapler validate "$DIST_DIR/$BASE.dmg"
  spctl -a -vvv -t open --context context:primary-signature "$DIST_DIR/$BASE.dmg"
fi

ls -l "$DIST_DIR/$BASE.dmg" "$DIST_DIR/$BASE.zip"
