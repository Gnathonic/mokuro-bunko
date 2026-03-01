#!/bin/bash
#
# Build standalone binary for mokuro-bunko
#
# This script creates a self-contained binary using shiv that includes
# all Python dependencies but can still install OCR dependencies at runtime.
#
# Usage:
#   ./scripts/build-binary.sh [--platform PLATFORM]
#
# Options:
#   --platform    Target platform: linux-x64, macos-arm64, windows-x64 (default: current)
#
# Requirements:
#   - Python 3.10+
#   - pip with shiv installed
#   - Build tools (gcc/clang for native dependencies)

set -euo pipefail

# Configuration
PROJECT_NAME="mokuro-bunko"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$PROJECT_ROOT/build"
DIST_DIR="$PROJECT_ROOT/dist"

# Detect current platform
detect_platform() {
    local os=$(uname -s | tr '[:upper:]' '[:lower:]')
    local arch=$(uname -m)

    case "$os" in
        linux)
            case "$arch" in
                x86_64) echo "linux-x64" ;;
                aarch64) echo "linux-arm64" ;;
                *) echo "linux-$arch" ;;
            esac
            ;;
        darwin)
            case "$arch" in
                x86_64) echo "macos-x64" ;;
                arm64) echo "macos-arm64" ;;
                *) echo "macos-$arch" ;;
            esac
            ;;
        mingw*|msys*|cygwin*)
            echo "windows-x64"
            ;;
        *)
            echo "unknown-$arch"
            ;;
    esac
}

# Parse arguments
PLATFORM=$(detect_platform)
while [[ $# -gt 0 ]]; do
    case $1 in
        --platform)
            PLATFORM="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

echo "Building $PROJECT_NAME for $PLATFORM"
echo "Project root: $PROJECT_ROOT"

# Create directories
mkdir -p "$BUILD_DIR" "$DIST_DIR"

# Ensure shiv is installed
if ! python -m shiv --version &>/dev/null; then
    echo "Installing shiv..."
    pip install shiv
fi

# Build wheel first
echo "Building wheel..."
cd "$PROJECT_ROOT"
python -m pip wheel --no-deps --wheel-dir="$BUILD_DIR" .

# Find the wheel
WHEEL=$(ls "$BUILD_DIR"/*.whl 2>/dev/null | head -1)
if [[ -z "$WHEEL" ]]; then
    echo "Error: No wheel found in $BUILD_DIR"
    exit 1
fi
echo "Built wheel: $WHEEL"

# Determine output binary name
case "$PLATFORM" in
    windows-*)
        OUTPUT_NAME="${PROJECT_NAME}-${PLATFORM}.exe"
        ;;
    *)
        OUTPUT_NAME="${PROJECT_NAME}-${PLATFORM}"
        ;;
esac
OUTPUT_PATH="$DIST_DIR/$OUTPUT_NAME"

# Build shiv archive
echo "Creating shiv archive..."
python -m shiv \
    --compressed \
    --python "/usr/bin/env python3" \
    --output-file "$OUTPUT_PATH" \
    --entry-point "mokuro_bunko.__main__:main" \
    "$WHEEL" \
    wsgidav \
    cheroot \
    pyyaml \
    bcrypt \
    watchdog \
    cryptography

# Make executable
chmod +x "$OUTPUT_PATH"

# Get file size
SIZE=$(du -h "$OUTPUT_PATH" | cut -f1)

echo ""
echo "Build complete!"
echo "Output: $OUTPUT_PATH"
echo "Size: $SIZE"
echo ""
echo "To run: $OUTPUT_PATH serve"
