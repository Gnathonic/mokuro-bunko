#!/bin/sh
# Build the Linux x86_64 `full` release archive and/or an OCR backend pack inside a
# manylinux_2_28 container (AlmaLinux 8: glibc 2.28, gcc-toolset-14), so they run on
# glibc >= 2.28 with the distribution's own libstdc++ (Debian 12, Ubuntu 22.04, RHEL 8+),
# as 0.5.2's pip wheels did. gcc-toolset's libstdc++ is a linker script that adds
# libstdc++_nonshared.a: the newer C++ runtime parts that ONNX Runtime 1.28's static
# libraries need (GLIBCXX_3.4.31 `_M_replace_cold`, ...) are linked statically and only
# the old GLIBCXX versions are taken from the system libstdc++.so.6.
#
#   docker run --rm --network host -v "$PWD:/src" -w /src \
#     -e CARGO_HOME=/src/target/manylinux/cargo -e RUSTUP_HOME=/src/target/manylinux/rustup \
#     -e CARGO_TARGET_DIR=/src/target/manylinux/build \
#     quay.io/pypa/manylinux_2_28_x86_64 packaging/manylinux/build.sh full [cpu cu130 rocm7.1]
#
# Arguments: `full` (xtask dist, the full archive, with the desktop tray), `tray` (just
# the tray executable, $OUT/mokuro-bunko-tray, for the musl lite archive's `--tray-bin`)
# and/or pack variants (xtask torch-pack). Output in $OUT (default dist/). The release workflow runs it as a job
# `container:`; it needs network access (rustup, crates, libtorch, nasm).
set -eu

OUT="${OUT:-dist}"
TARGET=x86_64-unknown-linux-gnu

# mozjpeg (libjpeg-turbo) builds its SIMD code with nasm: same decoder as 0.5.2's Pillow.
if ! command -v nasm >/dev/null 2>&1; then
	dnf -y -q install nasm || yum -y -q install nasm
fi
# The desktop tray (bunko-tray) links GTK 3; libayatana-appindicator is loaded at run time.
case " $* " in
*" full "* | *" tray "*)
	pkg-config --exists gtk+-3.0 2>/dev/null || dnf -y -q install gtk3-devel || yum -y -q install gtk3-devel
	;;
esac
if ! command -v cargo >/dev/null 2>&1; then
	if [ ! -x "${CARGO_HOME:-$HOME/.cargo}/bin/cargo" ]; then
		curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal
	fi
	PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
	export PATH
fi
gcc --version | head -n1
ldd --version | head -n1

# ONNX Runtime's prebuilt static libraries also reference four glibc >= 2.32/2.38
# symbols; glibc_compat.c provides them (see there). xtask dist appends its own flags.
mkdir -p "${CARGO_TARGET_DIR:-target}"
COMPAT="$(cd "${CARGO_TARGET_DIR:-target}" && pwd)/glibc_compat.o"
gcc -O2 -fPIC -c packaging/manylinux/glibc_compat.c -o "$COMPAT"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=$COMPAT"

LOCKED="${LOCKED:---locked}"
for what in "$@"; do
	case "$what" in
	full)
		cargo run --release ${LOCKED:+"$LOCKED"} -p xtask -- dist ${LOCKED:+"$LOCKED"} --target "$TARGET" --flavor full --out "$OUT"
		;;
	tray)
		cargo build --release ${LOCKED:+"$LOCKED"} -p bunko-tray --bin mokuro-bunko-tray --target "$TARGET"
		mkdir -p "$OUT"
		cp "${CARGO_TARGET_DIR:-target}/$TARGET/release/mokuro-bunko-tray" "$OUT/"
		echo "$OUT/mokuro-bunko-tray: $(objdump -T "$OUT/mokuro-bunko-tray" | grep -oE 'GLIBC_[0-9.]+' | sort -Vu | tail -n1)"
		;;
	*)
		cargo run --release ${LOCKED:+"$LOCKED"} -p xtask -- torch-pack ${LOCKED:+"$LOCKED"} --variant "$what" --target "$TARGET" --out "$OUT"
		;;
	esac
done

# What the binaries need from the system: glibc <= 2.28 and GLIBCXX <= 3.4.25 (GCC 8).
for f in "$OUT"/mokuro-bunko-*-"$TARGET"-full.tar.gz; do
	[ -e "$f" ] || continue
	d=$(mktemp -d)
	tar -xzf "$f" -C "$d"
	echo "$f: $(objdump -T "$d"/*/mokuro-bunko | grep -oE 'GLIBC_[0-9.]+|GLIBCXX_[0-9.]+' | sort -Vu | tr '\n' ' ')"
	rm -rf "$d"
done
