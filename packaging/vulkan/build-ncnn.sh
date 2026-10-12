#!/usr/bin/env bash
# Build the static ncnn (Vulkan, glslang) the Vulkan backend pack links:
#   packaging/vulkan/build-ncnn.sh <work dir>   → <work dir>/install (BUNKO_NCNN_DIR)
# Pinned to the tag the pack's custom layers were written against (docs/rust-port/VULKAN.md).
set -euo pipefail
NCNN_TAG=20260526
NCNN_COMMIT=e54f7b1f88434e1d844ea0551b880a1cfb079ce1
work=${1:?work dir}
mkdir -p "$work"
cd "$work"
if [ ! -d ncnn ]; then
  git clone --depth 1 --branch "$NCNN_TAG" --recurse-submodules --shallow-submodules \
    https://github.com/Tencent/ncnn.git ncnn
fi
[ "$(git -C ncnn rev-parse HEAD)" = "$NCNN_COMMIT" ] || { echo "ncnn $NCNN_TAG is not $NCNN_COMMIT" >&2; exit 1; }
# OpenMP only on Linux (ncnn's CPU layers; the GPU path never uses it): MSVC's would add a
# vcomp runtime the pack does not ship.
openmp=ON
case "$(uname -s)" in MINGW* | MSYS* | CYGWIN*) openmp=OFF ;; esac
# CMAKE_INSTALL_LIBDIR=lib: manylinux (RHEL) would otherwise install into lib64, where the
# bunko-ncnn build script does not look.
cmake -S ncnn -B build -G Ninja -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_INSTALL_PREFIX="$PWD/install" -DCMAKE_INSTALL_LIBDIR=lib -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
  -DNCNN_VULKAN=ON -DNCNN_SIMPLEVK=ON -DNCNN_SYSTEM_GLSLANG=OFF -DNCNN_OPENMP=$openmp \
  -DNCNN_SHARED_LIB=OFF -DNCNN_BUILD_TOOLS=OFF -DNCNN_BUILD_EXAMPLES=OFF \
  -DNCNN_BUILD_BENCHMARK=OFF -DNCNN_BUILD_TESTS=OFF -DNCNN_PYTHON=OFF
cmake --build build --parallel
cmake --install build
echo "BUNKO_NCNN_DIR=$PWD/install"
