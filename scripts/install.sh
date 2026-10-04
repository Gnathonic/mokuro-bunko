#!/bin/sh
# Install mokuro-bunko from a GitHub release (Linux, macOS).
#
#   curl -fsSL https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --flavor lite --systemd
#
# What it does:
#   1. downloads release.json + release.json.sig of the latest (or --version) release
#      and checks the ed25519 signature with the release public key (needs OpenSSL 3;
#      without it the signature is skipped with a warning, the download is still
#      checked against the manifest's sha256 over HTTPS);
#   2. picks the archive for this OS/CPU and flavor, downloads it and checks its sha256;
#   3. unpacks it into ~/.local/lib/mokuro-bunko (as root: /usr/local/lib/mokuro-bunko)
#      and links the executable into ~/.local/bin (/usr/local/bin);
#   4. with --systemd, installs and starts a systemd unit (a user unit, or as root a
#      system unit running as the `mokuro` user).
#
# Options:
#   --flavor lite|full             default: full where it exists, else lite
#                                  (then run `mokuro-bunko install-ocr` for the OCR backend)
#   --version X.Y.Z                default: the latest release
#   --prefix DIR                   install under DIR/lib/mokuro-bunko and DIR/bin
#   --systemd                      install + enable the server unit
#   --processor                    install the OCR processor unit (needs a full flavor)
#   --require-signature            fail instead of warning when OpenSSL 3 is missing
#   --dry-run                      resolve and print what would be installed
#
# Environment: MOKURO_BUNKO_REPO (default Gnathonic/mokuro-bunko),
#              MOKURO_BUNKO_BASE_URL (where release.json lives; default: the GitHub
#              release), MOKURO_BUNKO_PUBLIC_KEY (a fork's base64 ed25519 public key,
#              the same value as bunko-update's BUNKO_RELEASE_PUBLIC_KEY).
set -eu

REPO="${MOKURO_BUNKO_REPO:-Gnathonic/mokuro-bunko}"
# The ed25519 release public key compiled into bunko-update (RELEASE_PUBLIC_KEY).
PUBKEY="${MOKURO_BUNKO_PUBLIC_KEY:-bvEsRQhVlCH124jKyBWh5TAl7tl5SXHXhdFGJCs3HEk=}"

FLAVOR=""
VERSION=""
PREFIX=""
SYSTEMD=0
PROCESSOR=0
REQUIRE_SIG=0
DRY_RUN=0

say() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
	printf 'error: %s\n' "$*" >&2
	exit 1
}

while [ $# -gt 0 ]; do
	case "$1" in
	--flavor) FLAVOR="${2:?--flavor needs a value}"; shift 2 ;;
	--flavor=*) FLAVOR="${1#*=}"; shift ;;
	--version) VERSION="${2:?--version needs a value}"; shift 2 ;;
	--version=*) VERSION="${1#*=}"; shift ;;
	--prefix) PREFIX="${2:?--prefix needs a value}"; shift 2 ;;
	--prefix=*) PREFIX="${1#*=}"; shift ;;
	--systemd) SYSTEMD=1; shift ;;
	--processor) PROCESSOR=1; shift ;;
	--require-signature) REQUIRE_SIG=1; shift ;;
	--dry-run) DRY_RUN=1; shift ;;
	-h | --help)
		sed -n '2,/^set -eu/p' "$0" 2>/dev/null | sed -e '/^set -eu/d' -e 's/^# \{0,1\}//'
		exit 0
		;;
	*) die "unknown option: $1 (see --help)" ;;
	esac
done
VERSION="${VERSION#v}"

# --- tools -------------------------------------------------------------------
if command -v curl >/dev/null 2>&1; then
	fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
	fetch() { wget -q -O "$2" "$1"; }
else
	die "curl or wget is required"
fi
if command -v sha256sum >/dev/null 2>&1; then
	sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
	sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
	die "sha256sum or shasum is required"
fi
command -v tar >/dev/null 2>&1 || die "tar is required"

# --- platform ----------------------------------------------------------------
os="$(uname -s)"
glibc=""
case "$(uname -m)" in
x86_64 | amd64) arch=x86_64 ;;
aarch64 | arm64) arch=aarch64 ;;
*) die "unsupported CPU: $(uname -m) (releases exist for x86_64 and aarch64)" ;;
esac
case "$os" in
Linux)
	glibc=1
	if ldd --version 2>&1 | grep -qi musl || [ -e /lib/ld-musl-"$arch".so.1 ]; then glibc=0; fi
	;;
Darwin) ;;
*) die "unsupported OS: $os (Windows: scripts/install.ps1)" ;;
esac

triple_for() { # flavor -> target triple
	case "$os" in
	Darwin) echo "$arch-apple-darwin" ;;
	*) if [ "$1" = lite ]; then echo "$arch-unknown-linux-musl"; else echo "$arch-unknown-linux-gnu"; fi ;;
	esac
}

# --- manifest ----------------------------------------------------------------
if [ -n "${MOKURO_BUNKO_BASE_URL:-}" ]; then
	base="${MOKURO_BUNKO_BASE_URL%/}"
elif [ -n "$VERSION" ]; then
	base="https://github.com/$REPO/releases/download/v$VERSION"
else
	base="https://github.com/$REPO/releases/latest/download"
fi
tmp="$(mktemp -d 2>/dev/null || mktemp -d -t mokuro-bunko)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Fetching $base/release.json"
fetch "$base/release.json" "$tmp/release.json" || die "could not download release.json (no such release?)"
fetch "$base/release.json.sig" "$tmp/release.json.sig" || die "could not download release.json.sig"

verify_signature() {
	command -v openssl >/dev/null 2>&1 || return 2
	openssl version 2>/dev/null | grep -q '^OpenSSL [3-9]' || return 2
	# PEM = the fixed DER header of an ed25519 SubjectPublicKeyInfo + the 32 raw key bytes.
	{
		printf '\060\052\060\005\006\003\053\145\160\003\041\000'
		printf '%s' "$PUBKEY" | openssl base64 -d -A
	} >"$tmp/pub.der" || return 1
	[ "$(wc -c <"$tmp/pub.der" | tr -d ' ')" = 44 ] || return 1
	{
		echo "-----BEGIN PUBLIC KEY-----"
		openssl base64 -A -in "$tmp/pub.der"
		echo
		echo "-----END PUBLIC KEY-----"
	} >"$tmp/pub.pem"
	openssl base64 -d -A -in "$tmp/release.json.sig" -out "$tmp/sig.bin" 2>/dev/null || return 1
	openssl pkeyutl -verify -pubin -inkey "$tmp/pub.pem" -rawin -in "$tmp/release.json" -sigfile "$tmp/sig.bin" >/dev/null 2>&1
}
# The signature file is one base64 line; openssl's -A decoder wants no newline.
tr -d '\r\n' <"$tmp/release.json.sig" >"$tmp/sig.b64" && mv "$tmp/sig.b64" "$tmp/release.json.sig"
set +e
verify_signature
sig_status=$?
set -e
case "$sig_status" in
0) say "Signature OK" ;;
2)
	[ "$REQUIRE_SIG" = 1 ] && die "OpenSSL 3 is needed to check the release signature"
	warn "OpenSSL 3 not found: release.json signature NOT checked (the archive's sha256 still is)"
	;;
*) die "release.json does not carry a valid signature from the mokuro-bunko release key" ;;
esac

# release.json is pretty-printed by `xtask manifest`, one key per line:
#   "artifacts": { "<triple>": { "<flavor>": { "url": ..., "sha256": ..., ... } } }
# $1 = triple, $2 = flavor, $3 = field ("" lists the triple's flavors).
manifest_get() {
	awk -v t="$1" -v f="$2" -v k="$3" '
		/^  "[^"]*": / { split($0, a, "\""); top = a[2] }
		top != "artifacts" { next }
		/^    "[^"]*": \{/ { split($0, a, "\""); ct = a[2]; next }
		/^      "[^"]*": \{/ { split($0, a, "\""); cf = a[2]; if (k == "" && ct == t) print cf; next }
		/^        "[^"]*": / {
			split($0, a, "\"")
			if (k != "" && ct == t && cf == f && a[2] == k) {
				v = $0; sub(/^ *"[^"]*": */, "", v); sub(/,$/, "", v); gsub(/"/, "", v); print v; exit
			}
		}' "$tmp/release.json"
}
release_version="$(awk -F'"' '/^  "version": /{print $4; exit}' "$tmp/release.json")"
[ -n "$release_version" ] || die "release.json has no version"

if [ -z "$FLAVOR" ]; then
	if [ "$os" = Linux ] && [ "$glibc" = 0 ]; then
		FLAVOR=lite
	elif manifest_get "$(triple_for full)" "" "" | grep -qx full; then
		FLAVOR=full
	else
		FLAVOR=lite
	fi
fi
case "$FLAVOR" in
lite | full | full-*) ;;
*) die "--flavor must be lite or full" ;;
esac
if [ "$os" = Linux ] && [ "$FLAVOR" != lite ] && [ "$glibc" = 0 ]; then
	die "the $FLAVOR build needs glibc; this system uses musl (use --flavor lite)"
fi
triple="$(triple_for "$FLAVOR")"
url="$(manifest_get "$triple" "$FLAVOR" url)"
want_sha="$(manifest_get "$triple" "$FLAVOR" sha256)"
if [ -z "$url" ] || [ -z "$want_sha" ]; then
	have="$(manifest_get "$triple" "" "" | tr '\n' ' ')"
	die "release $release_version has no $FLAVOR build for $triple (available: ${have:-none})"
fi

# --- where -------------------------------------------------------------------
if [ -n "$PREFIX" ]; then
	:
elif [ "$(id -u)" = 0 ]; then
	PREFIX=/usr/local
else
	PREFIX="$HOME/.local"
fi
libdir="$PREFIX/lib/mokuro-bunko"
bindir="$PREFIX/bin"

say "mokuro-bunko $release_version ($FLAVOR, $triple)"
say "  from $url"
say "  into $libdir (linked from $bindir/mokuro-bunko)"
[ "$DRY_RUN" = 1 ] && exit 0

# --- download, verify, unpack ------------------------------------------------
archive="$tmp/$(basename "$url")"
say "Downloading..."
fetch "$url" "$archive" || die "download failed: $url"
got_sha="$(sha256 "$archive")"
[ "$got_sha" = "$want_sha" ] || die "sha256 mismatch: got $got_sha, the signed manifest says $want_sha"
say "Checksum OK"

mkdir -p "$tmp/x"
tar -xzf "$archive" -C "$tmp/x"
src="$(find "$tmp/x" -mindepth 1 -maxdepth 1 -type d | head -n 1)"
[ -n "$src" ] && [ -x "$src/mokuro-bunko" ] || die "the archive has no mokuro-bunko executable"

mkdir -p "$PREFIX/lib" "$bindir"
rm -rf "$libdir.new" "$libdir.old"
cp -R "$src" "$libdir.new"
# Run it before replacing anything (e.g. a glibc too old for a full build).
if ! "$libdir.new/mokuro-bunko" --version; then
	rm -rf "$libdir.new"
	die "the $FLAVOR build does not run on this system; nothing was changed${glibc:+ (a full build needs glibc 2.28+; try --flavor lite)}"
fi
if [ -d "$libdir" ]; then mv "$libdir" "$libdir.old"; fi
mv "$libdir.new" "$libdir"
rm -rf "$libdir.old"
ln -sf "$libdir/mokuro-bunko" "$bindir/mokuro-bunko"

case ":$PATH:" in
*":$bindir:"*) ;;
*) warn "$bindir is not on your PATH" ;;
esac

# --- systemd -----------------------------------------------------------------
write_units() { # $1 = system|user
	if [ "$1" = system ]; then
		unitdir=/etc/systemd/system
		if ! id mokuro >/dev/null 2>&1; then
			useradd --system --home-dir /var/lib/mokuro-bunko --shell /usr/sbin/nologin mokuro 2>/dev/null ||
				adduser --system --home /var/lib/mokuro-bunko mokuro
		fi
		mkdir -p /var/lib/mokuro-bunko /etc/mokuro-bunko
		chown mokuro: /var/lib/mokuro-bunko
		cat >"$unitdir/mokuro-bunko.service" <<EOF
# Installed by scripts/install.sh (source: deploy/mokuro-bunko.service)
[Unit]
Description=Mokuro Bunko manga library server
Documentation=https://github.com/$REPO
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=mokuro
Group=mokuro
WorkingDirectory=/var/lib/mokuro-bunko
Environment=MOKURO_HOST=127.0.0.1
Environment=MOKURO_PORT=8080
Environment=MOKURO_STORAGE=/var/lib/mokuro-bunko/storage
Environment=MOKURO_CONFIG=/var/lib/mokuro-bunko/config.yaml
# The binary is root-owned: the admin panel reports new releases, re-run install.sh to update.
Environment=MOKURO_INSTALL_KIND=install.sh
ExecStart=$bindir/mokuro-bunko serve
Restart=on-failure
RestartSec=5s
TimeoutStopSec=30s
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/mokuro-bunko
PrivateTmp=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
		if [ "$PROCESSOR" = 1 ]; then
			cat >"$unitdir/mokuro-bunko-processor.service" <<EOF
# Installed by scripts/install.sh (source: deploy/mokuro-bunko-processor.service)
[Unit]
Description=Mokuro Bunko OCR processor
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=mokuro
Group=mokuro
# GPU access: add mokuro to the video (and for AMD: render) group.
WorkingDirectory=/var/lib/mokuro-bunko
Environment=MOKURO_INSTALL_KIND=install.sh
ExecStart=$bindir/mokuro-bunko processor serve --config /etc/mokuro-bunko/processor.yaml
Restart=on-failure
RestartSec=10s
TimeoutStopSec=60s
NoNewPrivileges=yes
ProtectSystem=full
PrivateTmp=yes
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
		fi
		systemctl daemon-reload
		systemctl enable --now mokuro-bunko.service
		ctl="systemctl"
	else
		unitdir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
		mkdir -p "$unitdir"
		cat >"$unitdir/mokuro-bunko.service" <<EOF
# Installed by scripts/install.sh (source: deploy/mokuro-bunko.user.service)
[Unit]
Description=Mokuro Bunko manga library server
Documentation=https://github.com/$REPO
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
# Self-managed install: the admin panel can update the binary in place and restart.
ExecStart=$bindir/mokuro-bunko serve
Restart=on-failure
RestartSec=5s
TimeoutStopSec=30s

[Install]
WantedBy=default.target
EOF
		if [ "$PROCESSOR" = 1 ]; then
			cat >"$unitdir/mokuro-bunko-processor.service" <<EOF
# Installed by scripts/install.sh (source: deploy/mokuro-bunko-processor.user.service)
[Unit]
Description=Mokuro Bunko OCR processor
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
ExecStart=$bindir/mokuro-bunko processor serve --config %E/mokuro-bunko/processor.yaml
Restart=on-failure
RestartSec=10s
TimeoutStopSec=60s

[Install]
WantedBy=default.target
EOF
		fi
		systemctl --user daemon-reload
		systemctl --user enable --now mokuro-bunko.service
		ctl="systemctl --user"
		say "Tip: 'loginctl enable-linger $(id -un)' keeps it running after you log out."
	fi
	say "Service: $ctl status mokuro-bunko"
	if [ "$PROCESSOR" = 1 ]; then
		say "Processor unit installed (not started): write its config, then '$ctl enable --now mokuro-bunko-processor'"
	fi
}

if [ "$SYSTEMD" = 1 ] || [ "$PROCESSOR" = 1 ]; then
	command -v systemctl >/dev/null 2>&1 || die "--systemd needs systemd (on macOS run 'mokuro-bunko serve' or use launchd)"
	if [ "$(id -u)" = 0 ]; then write_units system; else write_units user; fi
fi

say ""
say "Installed mokuro-bunko $release_version ($FLAVOR)."
if [ "$SYSTEMD" != 1 ]; then
	say "Start it with:  mokuro-bunko serve   then open http://127.0.0.1:8080"
fi
