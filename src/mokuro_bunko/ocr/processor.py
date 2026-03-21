"""OCR processor for mokuro-bunko.

Handles running Mokuro on manga files and moving them to the library.
"""

from __future__ import annotations

import gzip
import json
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
import zipfile
from collections.abc import Callable
from io import BytesIO
from pathlib import Path
from typing import Any

from mokuro_bunko.ocr.installer import OCRInstaller

# Supported manga file extensions
SUPPORTED_EXTENSIONS = {".cbz", ".cbr", ".zip", ".rar"}


class OCRProcessor:
    """Processes manga files with Mokuro OCR."""

    def __init__(
        self,
        storage_path: Path,
        python_path: Path | None = None,
        status_callback: Callable[[str], None] | None = None,
        progress_callback: Callable[[dict[str, Any]], None] | None = None,
        hard_timeout_seconds: int = 3600,
        no_progress_timeout_seconds: int = 600,
        finalizing_timeout_seconds: int = 180,
    ) -> None:
        """Initialize the OCR processor.

        Args:
            storage_path: Base storage path containing inbox/ and library/.
            python_path: Path to Python executable with mokuro installed.
                        If None, auto-detects from OCRInstaller.
            status_callback: Optional callback for status messages.
            progress_callback: Optional callback for OCR progress updates.
        """
        self.storage_path = storage_path
        self.inbox_path = storage_path / "inbox"
        self.library_path = storage_path / "library"
        self.status_callback = status_callback or (lambda msg: None)
        self.progress_callback = progress_callback or (lambda data: None)
        self.hard_timeout_seconds = hard_timeout_seconds
        self.no_progress_timeout_seconds = no_progress_timeout_seconds
        self.finalizing_timeout_seconds = finalizing_timeout_seconds

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
                if not self._run_mokuro(extract_dir, workspace):
                    self._log(f"Mokuro failed for: {input_path.name}")
                    return False

                sidecar = self._collect_valid_workspace_sidecar(extract_dir, workspace)
                if sidecar is None:
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
            if not self._run_mokuro(input_path, temp_output):
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
        workspace = self._build_temp_workspace(cbz_path.stem)
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
            if not self._run_mokuro(extract_dir, workspace, total_images=total_images):
                sidecar = self._collect_valid_workspace_sidecar(extract_dir, workspace)
                if sidecar is None:
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
                        "error": "mokuro returned non-zero status and no valid sidecar was produced",
                    })
                    return False
                self._log(
                    f"Mokuro exited with error but sidecar was generated; importing for: {cbz_path.name}"
                )
            if sidecar is None:
                sidecar = self._collect_valid_workspace_sidecar(extract_dir, workspace)
            if sidecar is None:
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
                    "error": "no valid mokuro sidecar found in workspace",
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
            return True
        except Exception as e:
            self._log(f"Error processing {cbz_path.name}: {e}")
            self._emit_progress({
                "active": True,
                "series": str(cbz_path.parent.relative_to(self.library_path)) if cbz_path.exists() else None,
                "volume": cbz_path.stem,
                "relative_cbz": str(cbz_path.relative_to(self.library_path)) if cbz_path.exists() else None,
                "percent": 0,
                "eta_seconds": None,
                "done_pages": 0,
                "total_pages": None,
                "status": "error",
                "error": str(e),
            })
            return False
        finally:
            if workspace.exists():
                shutil.rmtree(workspace, ignore_errors=True)

    def process_library_thumbnail(self, cbz_path: Path) -> bool:
        """Generate missing thumbnail for a library CBZ."""
        if not cbz_path.exists():
            self._log(f"CBZ not found: {cbz_path}")
            return False
        if not self.needs_thumbnail(cbz_path):
            return True
        return self.ensure_thumbnail(cbz_path)

    def _run_mokuro(self, input_path: Path, output_dir: Path, total_images: int = 0) -> bool:
        """Run mokuro on the input file.

        Args:
            input_path: Path to manga file/folder.
            output_dir: Directory for mokuro output.

        Returns:
            True if mokuro succeeded.
        """
        try:
            hard_timeout_seconds = self.hard_timeout_seconds
            no_progress_timeout_seconds = self.no_progress_timeout_seconds
            finalizing_timeout_seconds = self.finalizing_timeout_seconds

            # Run mokuro with the OCR environment's Python
            cmd = [
                str(self.python_path),
                "-m",
                "mokuro",
                str(input_path),
                "--output-dir",
                str(output_dir),
                "--disable_confirmation",
                "--no_cache",
            ]

            self._log(f"Running: {' '.join(cmd)}")

            process = subprocess.Popen(
                cmd,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            start = time.time()
            last_done = -1
            last_progress_time = start
            finalizing_since: float | None = None

            while process.poll() is None:
                now = time.time()
                if now - start > hard_timeout_seconds:
                    process.kill()
                    self._log("Mokuro timed out")
                    return False
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
                            return True
                        process.kill()
                        self._log("Mokuro stalled in finalizing phase")
                        return False
                else:
                    finalizing_since = None

                if now - last_progress_time > no_progress_timeout_seconds:
                    process.kill()
                    self._log("Mokuro stalled with no OCR progress")
                    return False

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

            # Compatibility fallback for mokuro versions that do not support
            # --output-dir and always write sidecars next to the input.
            if result_code != 0:
                fallback_cmd = [
                    str(self.python_path),
                    "-m",
                    "mokuro",
                    str(input_path),
                    "--disable_confirmation",
                    "--no_cache",
                ]
                self._log(f"Retrying without --output-dir: {' '.join(fallback_cmd)}")
                process = subprocess.Popen(
                    fallback_cmd,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                start = time.time()
                last_done = -1
                last_progress_time = start
                finalizing_since = None
                while process.poll() is None:
                    now = time.time()
                    if now - start > hard_timeout_seconds:
                        process.kill()
                        self._log("Mokuro timed out")
                        return False
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
                                return True
                            process.kill()
                            self._log("Mokuro stalled in finalizing phase")
                            return False
                    else:
                        finalizing_since = None

                    if now - last_progress_time > no_progress_timeout_seconds:
                        process.kill()
                        self._log("Mokuro stalled with no OCR progress")
                        return False

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
                self._log("Mokuro error: subprocess exited with non-zero status")
                return False

            return True

        except subprocess.TimeoutExpired:
            self._log("Mokuro timed out")
            return False
        except FileNotFoundError:
            self._log(f"Python not found: {self.python_path}")
            return False
        except Exception as e:
            self._log(f"Mokuro exception: {e}")
            return False

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
