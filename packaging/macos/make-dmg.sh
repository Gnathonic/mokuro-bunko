#!/bin/sh
# Build the macOS disk image: "Mokuro Bunko.app" next to an Applications alias, on a
# background that says to drag one onto the other, plus a small "Read me".
#
#   packaging/macos/make-dmg.sh dist/mokuro-bunko-<ver>-aarch64-apple-darwin-full.tar.gz \
#       <ocr-offline dir> dist/mokuro-bunko-<ver>-macos-arm64.dmg
#
# The app is the release archive's mokuro-bunko.app (tray + CLI, `xtask dist`),
# renamed, with the offline OCR files (backend pack archive + model files: what
# `install-ocr --from` takes) in Contents/Resources/ocr-offline, where install-ocr and
# the setup wizard find them by themselves. Runs on macOS: hdiutil and dmgbuild
# (`pip install dmgbuild`, BSD licence; it writes the Finder layout without driving
# Finder). DMGBUILD may name its executable.
set -eu

[ $# -eq 3 ] || { echo "usage: $0 <full .tar.gz> <ocr-offline dir> <out.dmg>" >&2; exit 2; }
ARCHIVE=$1
OFFLINE=$2
OUT=$3
HERE=$(cd "$(dirname "$0")" && pwd)
DMGBUILD=${DMGBUILD:-dmgbuild}
APP="Mokuro Bunko.app"

ls "$OFFLINE"/*-torch-*.tar.zst >/dev/null 2>&1 || {
	echo "$OFFLINE holds no *-torch-*.tar.zst backend pack" >&2
	exit 1
}

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM
tar -xzf "$ARCHIVE" -C "$TMP"
TOP=$(find "$TMP" -mindepth 1 -maxdepth 1 -type d | head -n 1)
NAME=$(basename "$TOP")                                   # mokuro-bunko-<ver>-<target>-<flavor>
VERSION=$(echo "$NAME" | sed -E 's/^mokuro-bunko-(.*)-aarch64-apple-darwin-.*$/\1/; s/^mokuro-bunko-(.*)-x86_64-apple-darwin-.*$/\1/')
[ -d "$TOP/mokuro-bunko.app" ] || { echo "$ARCHIVE has no mokuro-bunko.app" >&2; exit 1; }

STAGE="$TMP/stage"
mkdir -p "$STAGE"
# cp -R makes the bundle's CLI (a hard link to the top-level one in the archive) a
# file of its own: the app carries its own copy wherever it is dragged.
cp -R "$TOP/mokuro-bunko.app" "$STAGE/$APP"
C="$STAGE/$APP/Contents"
[ -f "$C/MacOS/mokuro-bunko" ] || cp "$TOP/mokuro-bunko" "$C/MacOS/mokuro-bunko"
[ -f "$C/MacOS/mokuro-bunko-tray" ] || { echo "the app has no tray" >&2; exit 1; }
mkdir -p "$C/Resources"
cp -R "$OFFLINE" "$C/Resources/ocr-offline"
for f in LICENSE THIRD-PARTY-LICENSES.md README.md; do
	[ -f "$TOP/$f" ] && cp "$TOP/$f" "$C/Resources/$f"
done
sed "s/@VERSION@/$VERSION/g" "$HERE/Read me.txt" >"$STAGE/Read me.txt"
# Nothing from the build host's quarantine or Finder state.
xattr -cr "$STAGE" 2>/dev/null || true

cat >"$TMP/settings.py" <<EOF
# dmgbuild settings (see make-dmg.sh)
files = ["$STAGE/$APP", "$STAGE/Read me.txt"]
symlinks = {"Applications": "/Applications"}
icon = "$C/Resources/mokuro-bunko.icns"
background = "$HERE/dmg-background.png"
format = "UDZO"
compression_level = 9
filesystem = "HFS+"
window_rect = ((200, 140), (640, 400))
default_view = "icon-view"
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
icon_size = 96
text_size = 13
icon_locations = {
    "$APP": (160, 190),
    "Applications": (480, 190),
    "Read me.txt": (585, 345),
}
EOF
rm -f "$OUT"
"$DMGBUILD" -s "$TMP/settings.py" "Mokuro Bunko $VERSION" "$OUT"
hdiutil verify "$OUT" >/dev/null
ls -l "$OUT"
