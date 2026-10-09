#!/bin/sh
# Build the macOS disk image: "Mokuro Bunko.app" next to an Applications alias, on a
# background that says to drag one onto the other, plus a small "Read me".
#
#   packaging/macos/make-dmg.sh dist/mokuro-bunko-update-<ver>-macos.tar.gz \
#       dist/mokuro-bunko-<ver>-macos.dmg
#   packaging/macos/make-dmg.sh --bundle-ocr <ocr-offline dir> <full .tar.gz> <out.dmg>
#
# The release names the image mokuro-bunko-<ver>-macos.dmg (crates/xtask/src/names.rs,
# dmg_name); release-build.yml makes it from the arm64 full archive. A build without a
# release name (mokuro-bunko-<ver>-<target>-<flavor>.tar.gz) works too.
#
# The app is the release archive's mokuro-bunko.app (`xtask dist`: its main program is
# mokuro-bunko, which runs the tray when the Finder opens it), renamed, with the
# program as a file of its own, and sealed ad hoc (`codesign --force --deep -s -`) so
# that `codesign --verify` passes; the updater seals it again after it replaces files
# in it. By default it carries no OCR backend and no model: the setup wizard's OCR
# step (`install-ocr`) detects the Mac's hardware and downloads the backend pack of
# this release and the models of the enabled engines, as on Linux and Windows. That is
# the release build. A lite archive gives a lite app (a library server, no local OCR),
# whose "Read me" says so.
#
# --bundle-ocr DIR (opt-in: offline installs, local test builds) copies DIR, the
# offline OCR files (backend pack archive + model files: what `install-ocr --from`
# takes), into Contents/Resources/ocr-offline, where install-ocr and the wizard find
# them by themselves and download nothing.
#
# Runs on macOS: hdiutil and dmgbuild (`pip install dmgbuild`, BSD licence; it writes
# the Finder layout without driving Finder). DMGBUILD may name its executable.
# MAKE_DMG_DRY_RUN=1 stops after staging and prints the staging directory (kept), for
# checking the layout without hdiutil/dmgbuild.
set -eu

usage() {
	echo "usage: $0 [--bundle-ocr <ocr-offline dir>] <.tar.gz> <out.dmg>" >&2
	exit 2
}
OFFLINE=
if [ "${1:-}" = "--bundle-ocr" ]; then
	[ $# -ge 2 ] || usage
	OFFLINE=$2
	shift 2
fi
[ $# -eq 2 ] || usage
ARCHIVE=$1
OUT=$2
HERE=$(cd "$(dirname "$0")" && pwd)
DMGBUILD=${DMGBUILD:-dmgbuild}
APP="Mokuro Bunko.app"

if [ -n "$OFFLINE" ]; then
	ls "$OFFLINE"/*-torch-*.tar.zst >/dev/null 2>&1 || {
		echo "$OFFLINE holds no *-torch-*.tar.zst backend pack" >&2
		exit 1
	}
fi

TMP=$(mktemp -d)
if [ "${MAKE_DMG_DRY_RUN:-}" != "1" ]; then
	trap 'rm -rf "$TMP"' EXIT INT TERM
fi
tar -xzf "$ARCHIVE" -C "$TMP"
TOP=$(find "$TMP" -mindepth 1 -maxdepth 1 -type d | head -n 1)
NAME=$(basename "$TOP")   # mokuro-bunko-update-<ver>-macos, or mokuro-bunko-<ver>-<target>-<flavor>
VERSION=$(echo "$NAME" | sed -E 's/^mokuro-bunko-update-(.*)-macos$/\1/; s/^mokuro-bunko-(.*)-aarch64-apple-darwin-.*$/\1/; s/^mokuro-bunko-(.*)-x86_64-apple-darwin-.*$/\1/')
[ -d "$TOP/mokuro-bunko.app" ] || { echo "$ARCHIVE has no mokuro-bunko.app" >&2; exit 1; }
case "$NAME" in
*-x86_64-apple-darwin-*) ARCH="Intel" ;;
*) ARCH="Apple silicon" ;;
esac
case "$NAME" in
*-lite) FLAVOR=lite ;;
*) FLAVOR=full ;;
esac
case "$VERSION" in
*-*) PRE=1 ;;
*) PRE=0 ;;
esac
if [ "$FLAVOR" = lite ] && [ -n "$OFFLINE" ]; then
	echo "--bundle-ocr needs a full archive ($NAME is lite)" >&2
	exit 1
fi

STAGE="$TMP/stage"
mkdir -p "$STAGE"
# cp -R makes the bundle's CLI (a hard link to the top-level one in the archive) a
# file of its own: the app carries its own copy wherever it is dragged.
cp -R "$TOP/mokuro-bunko.app" "$STAGE/$APP"
C="$STAGE/$APP/Contents"
[ -f "$C/MacOS/mokuro-bunko" ] || cp "$TOP/mokuro-bunko" "$C/MacOS/mokuro-bunko"
grep -q '<string>mokuro-bunko</string>' "$C/Info.plist" || { echo "the app's main program is not mokuro-bunko" >&2; exit 1; }
mkdir -p "$C/Resources"
if [ -n "$OFFLINE" ]; then
	cp -R "$OFFLINE" "$C/Resources/ocr-offline"
	MODE=bundled
elif [ "$FLAVOR" = lite ]; then
	MODE=lite
else
	MODE=download
fi
for f in LICENSE THIRD-PARTY-LICENSES.md README.md; do
	[ -f "$TOP/$f" ] && cp "$TOP/$f" "$C/Resources/$f"
done
# "Read me": the paragraphs of this build (@IF_<mode>@ ... @END@ blocks: DOWNLOAD,
# BUNDLED, FULL = either of them, LITE; PRERELEASE for a version with a `-`).
awk -v mode="$MODE" -v version="$VERSION" -v arch="$ARCH" -v pre="$PRE" '
	/^@IF_DOWNLOAD@$/ { keep = (mode == "download"); inblock = 1; next }
	/^@IF_BUNDLED@$/ { keep = (mode == "bundled"); inblock = 1; next }
	/^@IF_FULL@$/ { keep = (mode != "lite"); inblock = 1; next }
	/^@IF_LITE@$/ { keep = (mode == "lite"); inblock = 1; next }
	/^@IF_PRERELEASE@$/ { keep = (pre == 1); inblock = 1; next }
	/^@END@$/ { inblock = 0; next }
	inblock && !keep { next }
	{ gsub(/@VERSION@/, version); gsub(/@ARCH@/, arch); print }
' "$HERE/Read me.txt" >"$STAGE/Read me.txt"
# Nothing from the build host's quarantine or Finder state.
xattr -cr "$STAGE" 2>/dev/null || true
# Seal the whole bundle (ad hoc: no Apple identity), so `codesign --verify` passes on
# the app as it comes out of the image. Skipped in a dry run off macOS.
if command -v codesign >/dev/null 2>&1; then
	codesign --force --deep -s - "$STAGE/$APP"
	codesign --verify --deep --strict "$STAGE/$APP"
fi
if [ "${MAKE_DMG_DRY_RUN:-}" = "1" ]; then
	echo "$STAGE"
	exit 0
fi

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
