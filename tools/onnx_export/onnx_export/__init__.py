"""Reproducible ONNX exports of the mokuro-bunko OCR recognizers.

Dev-only: run by a maintainer to build the ``models-v1`` GitHub release that
the Rust processor downloads (docs/rust-port/MODELS.md). Nothing here runs on
users' machines.
"""

from importlib.metadata import PackageNotFoundError, version

try:
    EXPORT_TOOL_VERSION = version("mokuro-bunko-onnx-export")
except PackageNotFoundError:  # running from a checkout without installing
    EXPORT_TOOL_VERSION = "1.0.0"

# The release every artifact of this tool version is published under.
RELEASE_TAG = "models-v1"
