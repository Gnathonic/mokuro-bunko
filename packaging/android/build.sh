#!/usr/bin/env bash
# Build the Mokuro Bunko APK: the lite server as libbunko_android.so (cargo ndk, per ABI),
# the licence notices, then `gradlew assembleRelease`.
#
#   packaging/android/build.sh [--debug] [--abi arm64-v8a] [--abi x86_64] [--out DIR]
#
# Environment:
#   ANDROID_HOME / ANDROID_SDK_ROOT   SDK (default ~/Android/Sdk)
#   ANDROID_NDK_HOME                  NDK (default: $ANDROID_HOME/ndk/$NDK_VERSION)
#   CARGO_TARGET_DIR                  cargo's target directory (default <repo>/target)
#   CARGO_LOCKED=1                    pass --locked to cargo (CI)
#   BUNKO_ANDROID_KEYSTORE[_PASSWORD], BUNKO_ANDROID_KEY_ALIAS, BUNKO_ANDROID_KEY_PASSWORD
#                                     release signing (else the debug key; see MOBILE.md)
#
# Output: <out>/mokuro-bunko-<version>-android.apk (default out: <repo>/dist).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
NDK_VERSION=29.0.14206865
MIN_SDK=26

variant=release
abis=()
out="$root/dist"
while [ $# -gt 0 ]; do
    case "$1" in
        --debug) variant=debug ;;
        --abi) abis+=("$2"); shift ;;
        --out) out="$2"; shift ;;
        -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done
[ ${#abis[@]} -gt 0 ] || abis=(arm64-v8a x86_64)

export ANDROID_HOME="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}}"
if [ -z "${ANDROID_NDK_HOME:-}" ]; then
    ANDROID_NDK_HOME="$ANDROID_HOME/ndk/$NDK_VERSION"
fi
export ANDROID_NDK_HOME
[ -d "$ANDROID_NDK_HOME" ] || { echo "NDK not found at $ANDROID_NDK_HOME (set ANDROID_NDK_HOME)" >&2; exit 1; }
command -v cargo-ndk >/dev/null || { echo "cargo-ndk is missing: cargo install cargo-ndk" >&2; exit 1; }

target_of() {
    case "$1" in
        arm64-v8a) echo aarch64-linux-android ;;
        x86_64) echo x86_64-linux-android ;;
        armeabi-v7a) echo armv7-linux-androideabi ;;
        x86) echo i686-linux-android ;;
        *) echo "unknown ABI $1" >&2; exit 2 ;;
    esac
}

locked=()
[ "${CARGO_LOCKED:-}" = "1" ] && locked=(--locked)
profile_flag=(--release)
[ "$variant" = debug ] && profile_flag=()

# 1. Native library per ABI into app/src/main/jniLibs/<abi>/ (cargo ndk -o does the copy).
jnilibs="$here/app/src/main/jniLibs"
rm -rf "$jnilibs"
ndk_targets=()
for abi in "${abis[@]}"; do
    rustup target list --installed 2>/dev/null | grep -qx "$(target_of "$abi")" \
        || rustup target add "$(target_of "$abi")"
    ndk_targets+=(-t "$abi")
done
(cd "$root" && cargo ndk "${ndk_targets[@]}" --platform "$MIN_SDK" -o "$jnilibs" \
    build "${profile_flag[@]}" "${locked[@]}" -p bunko-android)

# 2. Licence notices: xtask's collector for the lite graph on Android, plus the crates
#    only the JNI library uses. Fails on copyleft, like the desktop archives.
assets="$here/app/src/main/assets"
mkdir -p "$assets"
first_target=$(target_of "${abis[0]}")
(cd "$root" && cargo run -q "${locked[@]}" -p xtask -- licenses --target "$first_target" --flavor lite \
    --out "$assets/THIRD-PARTY-LICENSES.md")
python3 "$here/android_licenses.py" "$root" "$first_target" "$assets/THIRD-PARTY-LICENSES.md"

# 3. The APK.
task=assembleRelease
[ "$variant" = debug ] && task=assembleDebug
(cd "$here" && ./gradlew --no-daemon "$task")

version=$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$root/Cargo.toml" | head -n1)
mkdir -p "$out"
apk_dir="$here/app/build/outputs/apk/$variant"
apk=$(find "$apk_dir" -maxdepth 1 -name '*.apk' | head -n1)
[ -n "$apk" ] || { echo "no APK in $apk_dir" >&2; exit 1; }
name="mokuro-bunko-$version-android.apk"
[ "$variant" = debug ] && name="mokuro-bunko-$version-android-debug.apk"
cp "$apk" "$out/$name"
(cd "$out" && sha256sum "$name" > "$name.sha256")
echo "built $out/$name"
