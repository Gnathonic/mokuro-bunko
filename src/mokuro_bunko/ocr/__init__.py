"""OCR module for mokuro-bunko."""

from mokuro_bunko.ocr.installer import (
    OCRBackend,
    OCRInstaller,
    detect_hardware,
    get_backend_unavailable_reasons,
    get_recommended_backend,
    get_supported_backends,
)
from mokuro_bunko.ocr.processor import OCRProcessor
from mokuro_bunko.ocr.watcher import InboxWatcher, OCRWorker

__all__ = [
    "OCRBackend",
    "OCRInstaller",
    "OCRProcessor",
    "OCRWorker",
    "InboxWatcher",
    "detect_hardware",
    "get_supported_backends",
    "get_backend_unavailable_reasons",
    "get_recommended_backend",
]
