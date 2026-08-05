"""OCR processor for mokuro-bunko.

Handles running Mokuro on manga files and moving them to the library.
"""

from __future__ import annotations

import gzip
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
import zipfile
from collections.abc import Callable
from dataclasses import dataclass
from io import BytesIO
from pathlib import Path
from typing import Any, NamedTuple

from mokuro_bunko.config import OcrConfig
from mokuro_bunko.logging_setup import get_ocr_log_dir
from mokuro_bunko.ocr.installer import OCRInstaller

# Supported manga file extensions
SUPPORTED_EXTENSIONS = {".cbz", ".cbr", ".zip", ".rar"}


class MokuroRunResult(NamedTuple):
    """Outcome of one mokuro subprocess run."""

    ok: bool
    error: str | None = None
    log_path: Path | None = None

    def __bool__(self) -> bool:
        # A NamedTuple is always truthy; make `if not result:` mean failure.
        return self.ok


@dataclass
class OcrFailure:
    """Details of the most recent OCR failure, for callers to persist."""

    error: str
    log_file: str | None = None


# Patterns used to pull a human-readable reason out of mokuro's output.
# Final line of a Python traceback, e.g. "ValueError: Couldn't instantiate ..."
_TRACEBACK_FINAL_RE = re.compile(
    r"^(?:[A-Za-z_][\w.]*(?:Error|Exception)|KeyboardInterrupt|SystemExit|MemoryError)"
    r"(?::\s?.*)?$"
)
# Any "some.module.ExceptionClass: message" line — only trusted inside a
# "Traceback (most recent call last):" block, so arbitrarily named
# exceptions (e.g. mokuro's InvalidImage) are still caught. The exception
# is the last such line in the block.
_EXCEPTION_LINE_RE = re.compile(r"^[A-Za-z_][\w.]*:\s?.*$")
# loguru error lines, e.g. "2026-07-09 ... | ERROR | mokuro.run:run:142 - message"
_LOGURU_ERROR_RE = re.compile(r"\|\s*ERROR\s*\|.*?-\s*(?P<msg>.+)$")
# mokuro's per-run summary, e.g. "Processed successfully: 0/1"
_PROCESSED_RE = re.compile(r"Processed successfully:\s*(?P<done>\d+)/(?P<total>\d+)")


class OCRProcessor:
    """Processes manga files with Mokuro OCR."""

    def __init__(
        self,
        storage_path: Path,
        python_path: Path | None = None,
        status_callback: Callable[[str], None] | None = None,
        progress_callback: Callable[[dict[str, Any]], None] | None = None,
        ocr_config: OcrConfig | None = None,
    ) -> None:
        """Initialize the OCR processor.

        Args:
            storage_path: Base storage path containing inbox/ and library/.
            python_path: Path to Python executable with mokuro installed.
                        If None, auto-detects from OCRInstaller.
            status_callback: Optional callback for status messages.
            progress_callback: Optional callback for OCR progress updates.
            ocr_config: OCR settings. Defaults are used when omitted, so direct
                       construction (tests, CLI) keeps working.
        """
        self.storage_path = storage_path
        self.inbox_path = storage_path / "inbox"
        self.library_path = storage_path / "library"
        self.status_callback = status_callback or (lambda msg: None)
        self.progress_callback = progress_callback or (lambda data: None)
        self.ocr_config = ocr_config or OcrConfig()
        # Details of the most recent failure (set by processing methods when
        # they return False), so callers can persist/display the reason.
        self.last_failure: OcrFailure | None = None

        if python_path:
            self.python_path = python_path
        else:
            # Try to get Python from OCR installer
            installer = OCRInstaller()
            detected = installer.get_python_executable()
            self.python_path = detected or Path(sys.executable)

    def _log(self, message: str) -> None:
        """Log a status message."""
        self.status_callback(message)

    def _emit_progress(self, data: dict[str, Any]) -> None:
        """Emit OCR progress update."""
        self.progress_callback(data)

    def _record_failure(self, error: str | None, log_path: Path | None) -> None:
        """Remember the most recent failure so callers can persist it."""
        self.last_failure = OcrFailure(
            error=error or "unknown error",
            log_file=str(log_path) if log_path is not None else None,
        )

    @staticmethod
    def get_mokuro_sidecar_paths(cbz_path: Path) -> tuple[Path, Path]:
        """Return expected sidecar paths for a CBZ file."""
        base = cbz_path.with_suffix("")
        return Path(f"{base}.mokuro"), Path(f"{base}.mokuro.gz")

    @staticmethod
    def get_cover_path(cbz_path: Path) -> Path:
        """Return expected cover thumbnail path for a CBZ file."""
        return cbz_path.with_suffix(".webp")

    def needs_mokuro_sidecar(self, cbz_path: Path) -> bool:
        """Check whether a CBZ file is missing mokuro sidecar output."""
        if not cbz_path.is_file() or cbz_path.suffix.lower() != ".cbz":
            return False
        sidecar_plain, sidecar_gz = self.get_mokuro_sidecar_paths(cbz_path)
        return not sidecar_plain.exists() and not sidecar_gz.exists()

    @staticmethod
    def get_nocover_marker_path(cbz_path: Path) -> Path:
        """Return path for the marker that indicates thumbnail extraction was attempted but failed."""
        return cbz_path.with_suffix(".nocover")

    def needs_thumbnail(self, cbz_path: Path) -> bool:
        """Check whether a CBZ file is missing its cover thumbnail."""
        if not cbz_path.is_file() or cbz_path.suffix.lower() != ".cbz":
            return False
        if self.get_cover_path(cbz_path).exists():
            return False
        if self.get_nocover_marker_path(cbz_path).exists():
            return False
        return True

    def _extract_cover_image_data(self, cbz_path: Path) -> bytes | None:
        """Extract the first image (sorted by path) from a CBZ archive."""
        image_extensions = {
            ".jpg",
            ".jpeg",
            ".png",
            ".gif",
            ".bmp",
            ".webp",
            ".tiff",
            ".tif",
        }
        try:
            with zipfile.ZipFile(cbz_path, "r") as zip_file:
                image_files = sorted(
                    name
                    for name in zip_file.namelist()
                    if Path(name).suffix.lower() in image_extensions
                )
                if not image_files:
                    return None
                with zip_file.open(image_files[0]) as img_file:
                    return img_file.read()
        except (zipfile.BadZipFile, OSError, KeyError):
            return None

    def ensure_thumbnail(self, cbz_path: Path) -> bool:
        """Generate a WebP thumbnail constrained within 250x350 preserving aspect ratio."""
        if not self.needs_thumbnail(cbz_path):
            return True

        image_data = self._extract_cover_image_data(cbz_path)
        if image_data is None:
            self._log(f"No cover image found in: {cbz_path.name}")
            self.get_nocover_marker_path(cbz_path).touch()
            return False

        try:
            from PIL import Image, ImageOps
        except ImportError:
            self._log("Thumbnail generation unavailable: Pillow is not installed")
            return False

        output_path = self.get_cover_path(cbz_path)
        try:
            with Image.open(BytesIO(image_data)) as img:
                # Preserve source aspect ratio while constraining to max bounds.
                thumb = ImageOps.contain(img.convert("RGB"), (250, 350), method=Image.Resampling.LANCZOS)
                thumb.save(output_path, format="WEBP", quality=85, method=6)
            self._log(f"Created thumbnail: {output_path.name}")
            return True
        except Exception as e:
            self._log(f"Failed to generate thumbnail for {cbz_path.name}: {e}")
            return False

    def _build_temp_workspace(self, name_hint: str) -> Path:
        """Create isolated temporary workspace for processing."""
        processing_root = self.storage_path / ".processing"
        processing_root.mkdir(parents=True, exist_ok=True)
        return Path(tempfile.mkdtemp(prefix=f"{name_hint}_", dir=str(processing_root)))

    def _workspace_for_volume(self, cbz_path: Path) -> Path:
        """Return the stable workspace directory for a library volume.

        Deterministic so a retried run finds the previous attempt's ``_ocr``
        cache and resumes rather than re-OCRing every page. Hashed rather than
        derived from the path directly because series and volume names contain
        spaces, separators and non-ASCII characters that do not survive being
        used as a directory name across platforms.
        """
        try:
            key = cbz_path.relative_to(self.library_path).as_posix()
        except ValueError:
            # Outside the library (direct call, tests): fall back to the full
            # path so the key is still unique.
            key = cbz_path.as_posix()
        digest = hashlib.sha256(key.encode("utf-8")).hexdigest()[:16]

        processing_root = self.storage_path / ".processing"
        processing_root.mkdir(parents=True, exist_ok=True)
        workspace = processing_root / digest
        workspace.mkdir(parents=True, exist_ok=True)
        return workspace

    def _release_workspace(self, workspace: Path, succeeded: bool) -> None:
        """Reclaim a workspace after a run, unless it may still be useful.

        A failed run's workspace holds the per-page OCR cache for the pages that
        did complete. Deleting it would force the retry to start from page 1,
        which is the whole problem this exists to avoid.
        """
        if not succeeded:
            self._log(f"Retaining workspace for resume: {workspace}")
            return
        if workspace.exists():
            shutil.rmtree(workspace, ignore_errors=True)

    def sweep_stale_workspaces(self) -> int:
        """Remove retained workspaces older than the retention window.

        Retaining a failed volume's workspace trades disk for time. This bounds
        that trade: a volume abandoned by the operator (deleted, or permanently
        failing) stops consuming disk after ``workspace_retention_days``.

        Returns:
            Number of workspaces removed.
        """
        processing_root = self.storage_path / ".processing"
        if not processing_root.is_dir():
            return 0

        cutoff = time.time() - (self.ocr_config.workspace_retention_days * 86400)
        removed = 0
        for entry in processing_root.iterdir():
            if not entry.is_dir():
                continue
            try:
                if entry.stat().st_mtime >= cutoff:
                    continue
            except OSError:
                continue
            shutil.rmtree(entry, ignore_errors=True)
            if not entry.exists():
                removed += 1
                self._log(f"Swept stale OCR workspace: {entry.name}")
        return removed

    def _extract_and_clean(self, cbz_path: Path, workspace: Path) -> Path:
        """Extract a CBZ into the workspace and remove embedded thumbnails.

        Some uploaders embed a .webp thumbnail named after the archive
        (e.g. ``Volume 01.webp`` inside ``Volume 01.cbz``).  These confuse
        mokuro into treating them as manga pages.  After extraction the
        matching .webp is deleted so mokuro never sees it.

        Returns the path to the extracted directory.
        """
        extract_dir = workspace / cbz_path.stem
        with zipfile.ZipFile(cbz_path, "r") as zf:
            zf.extractall(extract_dir)

        # Remove embedded thumbnail: top-level .webp matching the archive stem.
        thumb = extract_dir / f"{cbz_path.stem}.webp"
        if thumb.exists():
            self._log(f"Removing embedded thumbnail: {thumb.name}")
            thumb.unlink()

        return extract_dir

    def _collect_workspace_sidecar(self, temp_cbz_path: Path, workspace: Path) -> Path | None:
        """Find generated sidecar in temporary workspace."""
        stem = temp_cbz_path.stem
        candidates = sorted(
            p for p in workspace.rglob(f"{stem}.mokuro*")
            if p.is_file()
        )
        if not candidates:
            return None
        # Prefer sidecars written at workspace root for cleaner import semantics.
        root_candidates = [p for p in candidates if p.parent == workspace]
        if root_candidates:
            candidates = root_candidates
        preferred = next((p for p in candidates if p.name.endswith(".mokuro.gz")), candidates[0])
        return preferred

    @staticmethod
    def is_valid_mokuro_sidecar(sidecar_path: Path) -> bool:
        """Check whether a mokuro sidecar is parseable JSON."""
        if not sidecar_path.exists() or not sidecar_path.is_file():
            return False
        try:
            if sidecar_path.name.endswith(".mokuro.gz"):
                with gzip.open(sidecar_path, "rt", encoding="utf-8") as f:
                    json.load(f)
            else:
                with sidecar_path.open("r", encoding="utf-8") as f:
                    json.load(f)
            return True
        except (OSError, UnicodeDecodeError, json.JSONDecodeError, gzip.BadGzipFile):
            return False

    def _collect_valid_workspace_sidecar(self, temp_cbz_path: Path, workspace: Path) -> Path | None:
        """Find generated sidecar in temporary workspace that is valid JSON."""
        stem = temp_cbz_path.stem
        candidates = sorted(
            p for p in workspace.rglob(f"{stem}.mokuro*")
            if p.is_file()
        )
        if not candidates:
            return None
        root_candidates = [p for p in candidates if p.parent == workspace]
        if root_candidates:
            candidates = root_candidates
        ordered = sorted(candidates, key=lambda p: (not p.name.endswith(".mokuro.gz"), str(p)))
        for candidate in ordered:
            if self.is_valid_mokuro_sidecar(candidate):
                return candidate
            self._log(f"Ignoring corrupt mokuro sidecar: {candidate.name}")
        return None

    def _count_archive_images(self, cbz_path: Path) -> int:
        """Count image files in a CBZ archive."""
        image_extensions = {".jpg", ".jpeg", ".png", ".webp", ".bmp", ".gif", ".tif", ".tiff"}
        try:
            with zipfile.ZipFile(cbz_path, "r") as zf:
                return sum(1 for name in zf.namelist() if Path(name).suffix.lower() in image_extensions)
        except (zipfile.BadZipFile, OSError):
            return 0

    @staticmethod
    def _count_directory_images(directory: Path) -> int:
        """Count image files in an extracted directory."""
        image_extensions = {".jpg", ".jpeg", ".png", ".webp", ".bmp", ".gif", ".tif", ".tiff"}
        return sum(1 for p in directory.rglob("*") if p.is_file() and p.suffix.lower() in image_extensions)

    def _derive_series_name(self, source_cbz_path: Path) -> str:
        """Derive stable series name from the source CBZ parent folder."""
        parent = source_cbz_path.parent
        if parent in (self.library_path, self.inbox_path):
            return source_cbz_path.stem
        name = parent.name.strip()
        return name or source_cbz_path.stem

    def _normalize_mokuro_metadata(self, sidecar_path: Path, source_cbz_path: Path) -> None:
        """Rewrite sidecar metadata to stable series/title UUID based on source folder."""
        try:
            if sidecar_path.suffix.lower() == ".gz":
                with gzip.open(sidecar_path, "rt", encoding="utf-8") as f:
                    data = json.load(f)
            else:
                with sidecar_path.open("r", encoding="utf-8") as f:
                    data = json.load(f)
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as e:
            self._log(f"Skipping metadata normalization for {sidecar_path.name}: {e}")
            return

        if not isinstance(data, dict):
            self._log(f"Skipping metadata normalization for {sidecar_path.name}: invalid JSON root")
            return

        series_name = self._derive_series_name(source_cbz_path)
        data["title"] = series_name
        data["volume"] = source_cbz_path.stem
        data["title_uuid"] = str(uuid.uuid5(uuid.NAMESPACE_DNS, series_name))

        try:
            if sidecar_path.suffix.lower() == ".gz":
                with gzip.open(sidecar_path, "wt", encoding="utf-8") as f:
                    json.dump(data, f, ensure_ascii=False, separators=(",", ":"))
            else:
                with sidecar_path.open("w", encoding="utf-8") as f:
                    json.dump(data, f, ensure_ascii=False, separators=(",", ":"))
        except OSError as e:
            self._log(f"Failed to write normalized metadata for {sidecar_path.name}: {e}")
            return

        self._log(f"Normalized sidecar metadata: {sidecar_path.name}")

    @staticmethod
    def _count_ocr_json_files(workspace: Path) -> int:
        """Count generated per-page OCR JSON files in workspace cache."""
        ocr_root = workspace / "_ocr"
        if not ocr_root.exists():
            return 0
        return sum(1 for _ in ocr_root.rglob("*.json"))

    @staticmethod
    def _progress_metrics(done: int, total_images: int, elapsed: float) -> tuple[int | None, int | None, str]:
        """Compute OCR progress metrics.

        Returns:
            tuple of (percent, eta_seconds, status)
        """
        if total_images <= 0:
            return None, None, "running"

        if done >= total_images:
            return 100, 0, "finalizing"

        percent = min(99, int((done / total_images) * 100))
        eta_seconds: int | None = None
        if done > 0:
            rate = done / max(elapsed, 1e-6)
            if rate > 0:
                eta_seconds = int((total_images - done) / rate)
        return percent, eta_seconds, "running"

    def is_processable(self, path: Path) -> bool:
        """Check if a path is a processable manga file or folder.

        Args:
            path: Path to check.

        Returns:
            True if the path can be processed.
        """
        if not path.exists():
            return False

        # Check for supported archive extensions
        if path.is_file():
            return path.suffix.lower() in SUPPORTED_EXTENSIONS

        # Check for directory with images
        if path.is_dir():
            image_extensions = {".jpg", ".jpeg", ".png", ".webp", ".gif"}
            images = [
                f for f in path.iterdir()
                if f.is_file() and f.suffix.lower() in image_extensions
            ]
            return len(images) > 0

        return False

    def process(self, input_path: Path) -> bool:
        """Process a manga file or folder.

        Args:
            input_path: Path to the manga in the inbox.

        Returns:
            True if processing succeeded.
        """
        if not input_path.exists():
            self._log(f"Input path does not exist: {input_path}")
            return False

        # If the file is already in the library, process in place and
        # only generate missing sidecars.
        if input_path.is_file() and input_path.suffix.lower() == ".cbz":
            try:
                in_library = input_path.resolve().is_relative_to(self.library_path.resolve())
            except ValueError:
                in_library = False
            if in_library:
                return self.process_library_cbz(input_path)

        if not self.is_processable(input_path):
            self._log(f"Not a processable manga: {input_path}")
            return False

        self._log(f"Processing: {input_path.name}")

        if input_path.is_file() and input_path.suffix.lower() == ".cbz":
            workspace = self._build_temp_workspace(input_path.stem)
            try:
                extract_dir = self._extract_and_clean(input_path, workspace)
                run = self._run_mokuro(extract_dir, workspace)
                if not run.ok:
                    self._record_failure(run.error, run.log_path)
                    self._log(f"Mokuro failed for: {input_path.name}")
                    return False

                sidecar = self._collect_valid_workspace_sidecar(extract_dir, workspace)
                if sidecar is None:
                    self._record_failure("no valid .mokuro sidecar generated", run.log_path)
                    self._log(f"No valid mokuro sidecar generated for: {input_path.name}")
                    return False
                self._normalize_mokuro_metadata(sidecar, input_path)

                dest_path = self.library_path / input_path.name
                if dest_path.exists():
                    dest_path = self._get_unique_path(dest_path)
                shutil.move(str(input_path), str(dest_path))
                self._log(f"Moved to library: {dest_path.name}")

                suffix = ".mokuro.gz" if sidecar.name.endswith(".mokuro.gz") else ".mokuro"
                sidecar_dest = Path(f"{dest_path.with_suffix('')}{suffix}")
                if sidecar_dest.exists():
                    sidecar_dest = self._get_unique_path(sidecar_dest)
                shutil.move(str(sidecar), str(sidecar_dest))
                self._log(f"Created: {sidecar_dest.name}")

                self.ensure_thumbnail(dest_path)
                return True
            except Exception as e:
                self._log(f"Error processing {input_path.name}: {e}")
                return False
            finally:
                if workspace.exists():
                    shutil.rmtree(workspace, ignore_errors=True)

        # Create temporary output directory for mokuro
        temp_output = self.storage_path / ".processing" / input_path.stem
        temp_output.mkdir(parents=True, exist_ok=True)
        try:
            # Run mokuro
            run = self._run_mokuro(input_path, temp_output)
            if not run.ok:
                self._record_failure(run.error, run.log_path)
                self._log(f"Mokuro failed for: {input_path.name}")
                return False

            # Keep any sidecars generated adjacent to the source file.
            generated_sidecars = [p for p in self.get_mokuro_sidecar_paths(input_path) if p.exists()]
            temp_sidecars = [
                p
                for p in sorted(temp_output.glob("*.mokuro*"))
                if p.is_file() and (p.name.endswith(".mokuro") or p.name.endswith(".mokuro.gz"))
            ]

            sidecars_to_move: list[Path] = []
            seen: set[str] = set()
            for sidecar in temp_sidecars + generated_sidecars:
                key = str(sidecar)
                if key in seen:
                    continue
                seen.add(key)
                if not sidecar.exists():
                    continue
                if not self.is_valid_mokuro_sidecar(sidecar):
                    self._log(f"Skipping corrupt mokuro sidecar: {sidecar.name}")
                    continue
                sidecars_to_move.append(sidecar)

            if not sidecars_to_move:
                self._log(f"No valid mokuro sidecar generated for: {input_path.name}")
                return False

            # Move original file to library
            dest_path = self.library_path / input_path.name
            if dest_path.exists():
                # Handle duplicate names
                dest_path = self._get_unique_path(dest_path)

            shutil.move(str(input_path), str(dest_path))
            self._log(f"Moved to library: {dest_path.name}")

            for sidecar in sidecars_to_move:
                if not sidecar.exists():
                    continue
                self._normalize_mokuro_metadata(sidecar, input_path)
                suffix = ".mokuro.gz" if sidecar.name.endswith(".mokuro.gz") else ".mokuro"
                sidecar_dest = Path(f"{dest_path.with_suffix('')}{suffix}")
                if sidecar_dest.exists():
                    sidecar_dest = self._get_unique_path(sidecar_dest)
                shutil.move(str(sidecar), str(sidecar_dest))
                self._log(f"Created: {sidecar_dest.name}")

            return True

        except Exception as e:
            self._log(f"Error processing {input_path.name}: {e}")
            return False

        finally:
            # Cleanup temp directory
            if temp_output.exists():
                shutil.rmtree(temp_output, ignore_errors=True)

    def process_library_cbz(self, cbz_path: Path) -> bool:
        """Process missing OCR assets for a CBZ already in the library."""
        ocr_ok = self.process_library_ocr(cbz_path)
        thumb_ok = self.process_library_thumbnail(cbz_path)
        return ocr_ok and thumb_ok

    def process_library_ocr(self, cbz_path: Path) -> bool:
        """Generate missing mokuro sidecar for a library CBZ."""
        if not cbz_path.exists():
            self._log(f"CBZ not found: {cbz_path}")
            return False
        if not self.needs_mokuro_sidecar(cbz_path):
            self._log(f"Sidecar already exists, skipping OCR: {cbz_path.name}")
            return True

        self._log(f"Processing library CBZ in temp workspace: {cbz_path}")
        workspace = self._workspace_for_volume(cbz_path)
        succeeded = False
        try:
            extract_dir = self._extract_and_clean(cbz_path, workspace)
            total_images = self._count_directory_images(extract_dir)
            rel_cbz = str(cbz_path.relative_to(self.library_path))
            series_rel = str(cbz_path.parent.relative_to(self.library_path))
            self._emit_progress({
                "active": True,
                "series": series_rel,
                "volume": cbz_path.stem,
                "relative_cbz": rel_cbz,
                "percent": 0,
                "eta_seconds": None,
                "done_pages": 0,
                "total_pages": total_images if total_images > 0 else None,
                "status": "running",
            })
            sidecar: Path | None = None
            run = self._run_mokuro(extract_dir, workspace, total_images=total_images)
            if not run.ok:
                sidecar = self._collect_valid_workspace_sidecar(extract_dir, workspace)
                if sidecar is None:
                    self._record_failure(run.error, run.log_path)
                    self._log(f"Mokuro failed for: {cbz_path.name}")
                    self._emit_progress({
                        "active": True,
                        "series": series_rel,
                        "volume": cbz_path.stem,
                        "relative_cbz": rel_cbz,
                        "percent": 0,
                        "eta_seconds": None,
                        "done_pages": 0,
                        "total_pages": total_images if total_images > 0 else None,
                        "status": "error",
                        "error": run.error,
                    })
                    return False
                self._log(
                    f"Mokuro exited with error but sidecar was generated; importing for: {cbz_path.name}"
                )
            if sidecar is None:
                sidecar = self._collect_valid_workspace_sidecar(extract_dir, workspace)
            if sidecar is None:
                self._record_failure("no valid .mokuro sidecar generated", run.log_path)
                self._log(f"No valid mokuro sidecar generated for: {cbz_path.name}")
                self._emit_progress({
                    "active": True,
                    "series": series_rel,
                    "volume": cbz_path.stem,
                    "relative_cbz": rel_cbz,
                    "percent": 0,
                    "eta_seconds": None,
                    "done_pages": 0,
                    "total_pages": total_images if total_images > 0 else None,
                    "status": "error",
                    "error": "no valid .mokuro sidecar generated",
                })
                return False
            self._normalize_mokuro_metadata(sidecar, cbz_path)
            sidecar_plain, sidecar_gz = self.get_mokuro_sidecar_paths(cbz_path)
            dest = sidecar_gz if sidecar.name.endswith(".mokuro.gz") else sidecar_plain
            if dest.exists():
                dest = self._get_unique_path(dest)
            shutil.move(str(sidecar), str(dest))
            self._log(f"Created sidecar: {dest.name}")
            self._emit_progress({
                "active": True,
                "series": series_rel,
                "volume": cbz_path.stem,
                "relative_cbz": rel_cbz,
                "percent": 100,
                "eta_seconds": 0,
                "done_pages": total_images if total_images > 0 else None,
                "total_pages": total_images if total_images > 0 else None,
                "status": "done",
            })
            self.last_failure = None
            succeeded = True
            return True
        except Exception as e:
            self._record_failure(str(e), None)
            self._log(f"Error processing {cbz_path.name}: {e}")
            return False
        finally:
            self._release_workspace(workspace, succeeded)

    def process_library_thumbnail(self, cbz_path: Path) -> bool:
        """Generate missing thumbnail for a library CBZ."""
        if not cbz_path.exists():
            self._log(f"CBZ not found: {cbz_path}")
            return False
        if not self.needs_thumbnail(cbz_path):
            return True
        return self.ensure_thumbnail(cbz_path)

    def _get_mokuro_log_path(self, input_path: Path) -> Path:
        """Return the per-volume mokuro log path (parent dirs created)."""
        log_dir = get_ocr_log_dir(self.storage_path)
        log_dir.mkdir(parents=True, exist_ok=True)
        # Sanitize the stem so odd volume names can't escape the log dir.
        safe_stem = re.sub(r'[<>:"/\\|?*]', "_", input_path.stem) or "volume"
        return log_dir / f"{safe_stem}.log"

    @staticmethod
    def _extract_mokuro_error(log_path: Path) -> str | None:
        """Pull a short human-readable failure reason from a mokuro log.

        Prefers the final line of the last Python traceback, then the last
        loguru ERROR line. Returns at most 300 characters.
        """
        try:
            text = log_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return None

        lines = [line.strip() for line in text.splitlines() if line.strip()]
        traceback_final: str | None = None
        loguru_error: str | None = None
        module_error: str | None = None
        in_traceback = False
        for line in lines:
            if line.startswith("Traceback (most recent call last)"):
                in_traceback = True
                continue
            if in_traceback and _EXCEPTION_LINE_RE.match(line):
                # Keep overwriting: the raised exception is the last
                # "Exc.Class: message" line in the traceback block.
                traceback_final = line
                continue
            if _TRACEBACK_FINAL_RE.match(line):
                traceback_final = line
            match = _LOGURU_ERROR_RE.search(line)
            if match:
                loguru_error = match.group("msg").strip()
            # runpy's "python.exe: No module named mokuro" (broken OCR env)
            if "No module named" in line:
                module_error = line

        best = traceback_final or loguru_error or module_error
        if best is None:
            return None
        return best[:300]

    @staticmethod
    def _mokuro_reported_failure(log_path: Path) -> bool:
        """Check whether mokuro's own summary reports zero processed volumes.

        mokuro exits 0 even when every volume fails, so the exit code alone
        cannot be trusted; the "Processed successfully: N/M" summary can.
        """
        try:
            text = log_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return False
        matches = _PROCESSED_RE.findall(text)
        if not matches:
            return False
        done, total = matches[-1]  # trust the last summary in the log
        return int(done) < int(total)

    def _hard_timeout_seconds(self, total_images: int) -> int:
        """Total time a volume is allowed before the run is killed.

        A flat timeout scales inversely with need: the longer the volume, the
        more certain the kill. On a CPU backend a 228-page volume can exceed an
        hour comfortably, so the budget is derived from page count with a floor
        for short volumes.

        This is not the hang detector — ``no_progress_timeout_seconds`` catches a
        wedged process within ten minutes regardless of volume length.
        """
        per_page = self.ocr_config.timeout_per_page_seconds * max(0, total_images)
        return max(self.ocr_config.timeout_minimum_seconds, per_page)

    def _build_mokuro_command(self, input_path: Path) -> list[str]:
        """Build the mokuro subprocess command line.

        ``--no_cache`` makes mokuro ignore the per-page results it wrote under
        ``_ocr`` on a previous attempt, so it is passed only when the operator
        explicitly wants a clean re-OCR.
        """
        cmd = [
            str(self.python_path),
            "-m",
            "mokuro",
            str(input_path),
            "--disable_confirmation",
        ]
        if not self.ocr_config.use_cache:
            cmd.append("--no_cache")
        return cmd

    def _run_mokuro(
        self, input_path: Path, output_dir: Path, total_images: int = 0
    ) -> MokuroRunResult:
        """Run mokuro on the input file.

        The subprocess's combined stdout/stderr is captured to a per-volume
        log file under ``<storage>/logs/ocr/`` so failures are diagnosable.

        Args:
            input_path: Path to manga file/folder.
            output_dir: Directory for mokuro output.
            total_images: Expected page count for progress reporting.

        Returns:
            MokuroRunResult with success flag, short error summary, and the
            path of the captured log.
        """
        log_path: Path | None = None
        try:
            hard_timeout_seconds = self._hard_timeout_seconds(total_images)
            no_progress_timeout_seconds = 600
            finalizing_timeout_seconds = 900

            cmd = self._build_mokuro_command(input_path)

            self._log(f"Running: {' '.join(cmd)}")

            log_path = self._get_mokuro_log_path(input_path)
            env = dict(os.environ)
            env["PYTHONIOENCODING"] = "utf-8"
            env["PYTHONUNBUFFERED"] = "1"

            with log_path.open("w", encoding="utf-8", errors="replace") as log_file:
                log_file.write(f"# mokuro run for: {input_path}\n# command: {' '.join(cmd)}\n\n")
                log_file.flush()
                process = subprocess.Popen(
                    cmd,
                    stdout=log_file,
                    stderr=subprocess.STDOUT,
                    env=env,
                )
                start = time.time()
                last_done = -1
                last_progress_time = start
                finalizing_since: float | None = None

                while process.poll() is None:
                    now = time.time()
                    if now - start > hard_timeout_seconds:
                        process.kill()
                        return self._fail_run("Mokuro timed out", log_path)
                    done = self._count_ocr_json_files(output_dir)
                    if done != last_done:
                        last_done = done
                        last_progress_time = now

                    percent, eta_seconds, progress_status = self._progress_metrics(
                        done=done,
                        total_images=total_images,
                        elapsed=now - start,
                    )
                    if progress_status == "finalizing":
                        if finalizing_since is None:
                            finalizing_since = now
                        if now - finalizing_since > finalizing_timeout_seconds:
                            if self._collect_valid_workspace_sidecar(input_path, output_dir) is not None:
                                process.kill()
                                self._log("Mokuro finalizing exceeded timeout; valid sidecar found, continuing")
                                return MokuroRunResult(True, None, log_path)
                            process.kill()
                            return self._fail_run("Mokuro stalled in finalizing phase", log_path)
                    else:
                        finalizing_since = None

                    if now - last_progress_time > no_progress_timeout_seconds:
                        process.kill()
                        return self._fail_run("Mokuro stalled with no OCR progress", log_path)

                    self._emit_progress({
                        "active": True,
                        "percent": percent,
                        "eta_seconds": eta_seconds,
                        "done_pages": done,
                        "total_pages": total_images if total_images > 0 else None,
                        "status": progress_status,
                    })
                    time.sleep(2.0)

                result_code = process.returncode

            if result_code != 0:
                detail = self._extract_mokuro_error(log_path)
                return self._fail_run(
                    detail or "subprocess exited with non-zero status", log_path
                )

            # mokuro exits 0 even when a volume fails; trust its own summary.
            if self._mokuro_reported_failure(log_path):
                detail = self._extract_mokuro_error(log_path)
                return self._fail_run(
                    detail or "mokuro reported the volume was not processed", log_path
                )

            return MokuroRunResult(True, None, log_path)

        except subprocess.TimeoutExpired:
            return self._fail_run("Mokuro timed out", log_path)
        except FileNotFoundError:
            return self._fail_run(f"Python not found: {self.python_path}", log_path)
        except Exception as e:
            return self._fail_run(f"Mokuro exception: {e}", log_path)

    def _fail_run(self, error: str, log_path: Path | None) -> MokuroRunResult:
        """Log a mokuro failure and build its result object."""
        if log_path is not None:
            self._log(f"Mokuro failed: {error} (full log: {log_path})")
        else:
            self._log(f"Mokuro failed: {error}")
        return MokuroRunResult(False, error, log_path)

    def _get_unique_path(self, path: Path) -> Path:
        """Get a unique path by adding a counter suffix.

        Args:
            path: Original path that may exist.

        Returns:
            Unique path that doesn't exist.
        """
        if not path.exists():
            return path

        stem = path.stem
        suffix = path.suffix
        parent = path.parent
        counter = 1

        while True:
            new_path = parent / f"{stem}_{counter}{suffix}"
            if not new_path.exists():
                return new_path
            counter += 1


def create_processor_from_config(
    storage_path: Path,
    status_callback: Callable[[str], None] | None = None,
) -> OCRProcessor:
    """Create an OCR processor from configuration.

    Args:
        storage_path: Base storage path.
        status_callback: Optional status callback.

    Returns:
        Configured OCRProcessor instance.
    """
    return OCRProcessor(
        storage_path=storage_path,
        status_callback=status_callback,
    )
