"""Which machine wrote each OCR sidecar, and the audit trail of every result.

Two records, kept for two different questions:

* the ``ocr_sidecars`` table (`Database.record_ocr_sidecar`): who wrote the
  sidecar that is on disk NOW. One row per file, replaced by a re-run and
  deleted when the file leaves the library. It is what the admin card's
  History counts per machine (`attribute_volumes`).
* the audit log: every result DELIVERED, written (`WRITTEN`) or refused
  (`REJECTED`, with the reason), actor = the processor's account. That is
  how a processor that keeps sending bad files shows up. It is pruned with
  the rest of the audit log; the table is not.

Nothing here may fail an OCR result: a database that cannot be written to
costs the record and a log line, never the sidecar.
"""

from __future__ import annotations

import gzip
import json
import logging
import re
from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from mokuro_bunko.database import Database
    from mokuro_bunko.ocr.generations import GenerationSpec

logger = logging.getLogger(__name__)

#: Audit actions. The target of both is the sidecar (``target_type`` below).
WRITTEN = "ocr_sidecar_written"
REJECTED = "ocr_sidecar_rejected"
TARGET_TYPE = "sidecar"
# `webdav.resources.PathMapper.READER_ROOT`: audit target paths are the
# library's virtual paths, like every other library event's, so one search
# for a volume finds its upload and its OCR together.
_READER_ROOT = "mokuro-reader"
# What `engine_runner` logs as it writes a volume ("... failed_pages=N ...").
_FAILED_PAGES = re.compile(rb"failed_pages=(\d+)")
_LOG_TAIL_BYTES = 256 * 1024


@dataclass(frozen=True)
class SidecarFacts:
    """What a delivered sidecar says about itself, read before it is normalized."""

    pages: int | None
    # Its ``ocr_engine`` block as the RUNNER wrote it (the server's own
    # stamp is added later, and must not pass for the runner's).
    engine_block: Mapping[str, Any] = field(default_factory=dict)
    # The top-level mokuro ``version`` (mokuro's own for a monolithic or
    # served engine; the format version for a composed one).
    format_version: str | None = None


@dataclass(frozen=True)
class WrittenSidecar:
    """What a one-volume local run just installed (`OCRProcessor.last_written`)."""

    path: Path
    facts: SidecarFacts | None
    failed_pages: int | None


def read_sidecar_facts(path: Path) -> SidecarFacts | None:
    """The facts of a sidecar file, or None when it is missing or unreadable."""
    try:
        if path.name.endswith(".gz"):
            with gzip.open(path, "rt", encoding="utf-8") as handle:
                data = json.load(handle)
        else:
            with path.open("r", encoding="utf-8") as handle:
                data = json.load(handle)
    except (OSError, UnicodeDecodeError, ValueError, EOFError):
        return None
    if not isinstance(data, dict):
        return None
    pages = data.get("pages")
    block = data.get("ocr_engine")
    version = data.get("version")
    return SidecarFacts(
        pages=len(pages) if isinstance(pages, list) else None,
        engine_block=dict(block) if isinstance(block, dict) else {},
        format_version=version if isinstance(version, str) and version else None,
    )


def failed_pages_from_log(log_path: Path | None) -> int | None:
    """The runner's own count of pages it could not read, from its log.

    The LAST ``failed_pages=`` line of the log's tail (the runner writes one
    as it writes the volume). None when there is no such line -- the mokuro
    CLI prints none -- because a blank page in the file does not say whether
    the page failed or was simply empty.
    """
    if log_path is None:
        return None
    try:
        with log_path.open("rb") as handle:
            handle.seek(0, 2)
            size = handle.tell()
            handle.seek(max(0, size - _LOG_TAIL_BYTES))
            tail = handle.read()
    except OSError:
        return None
    found = _FAILED_PAGES.findall(tail)
    return int(found[-1]) if found else None


def runner_build(
    *,
    bunko_version: str | None,
    runner_digest: str | None,
    uses_mokuro: bool,
    facts: SidecarFacts | None,
) -> str | None:
    """"mokuro-bunko 0.5.0, runner 1a2b3c4d" -- what software wrote the file.

    ``runner_digest`` is the staged runner's content hash (`ocr.staging`):
    the exact build of the code that read the pages, which a version number
    alone does not pin down between releases. A row whose text comes from
    mokuro (monolithic or served) adds mokuro's own version from the file.
    """
    parts: list[str] = []
    if bunko_version:
        parts.append(f"mokuro-bunko {bunko_version}")
    if runner_digest:
        parts.append(f"runner {runner_digest}")
    if uses_mokuro and facts is not None and facts.format_version:
        parts.append(f"mokuro {facts.format_version}")
    return ", ".join(parts) or None


def attribute_volumes(
    records: Iterable[tuple[str, str, str]],
    present: Mapping[str, set[str]],
) -> dict[str, dict[str, int]]:
    """Volumes per (row, machine) whose sidecar is on disk: the History counts.

    ``records`` is ``(generation_id, volume_key, machine)``, oldest write
    first (`Database.ocr_sidecar_producers`); ``present`` is, per row, the
    volumes that have its sidecar now. A volume counts once, for the machine
    of its NEWEST record, and only while its sidecar is there -- so the
    machines of a row sum to at most its done count, and so to at most the
    library's total. A sidecar with no record counts for nobody.
    """
    newest: dict[tuple[str, str], str] = {}
    for generation_id, volume_key, machine in records:
        newest[(generation_id, volume_key)] = machine
    counts: dict[str, dict[str, int]] = {}
    for (generation_id, volume_key), machine in newest.items():
        if volume_key not in present.get(generation_id, ()):
            continue
        by_machine = counts.setdefault(generation_id, {})
        by_machine[machine] = by_machine.get(machine, 0) + 1
    return counts


class ProvenanceRecorder:
    """Writes the table row and the audit events for one library."""

    def __init__(self, database: Database, library_path: Path) -> None:
        self.database = database
        self.library_path = Path(library_path)

    def _relative(self, path: Path) -> str | None:
        try:
            return path.resolve().relative_to(self.library_path.resolve()).as_posix()
        except (OSError, ValueError):
            return None

    def _target(self, relative: str | None, path: Path) -> str:
        return f"/{_READER_ROOT}/{relative}" if relative is not None else str(path)

    def written(
        self,
        *,
        sidecar: Path,
        cbz: Path,
        generation: GenerationSpec,
        machine: str,
        account: str | None,
        facts: SidecarFacts | None,
        pages: int | None,
        failed_pages: int | None,
        build: str | None,
        archive_stamp: tuple[int, int] | None,
    ) -> None:
        """A sidecar was just installed at ``sidecar``: record it and audit it."""
        block = facts.engine_block if facts is not None else {}
        detector = block.get("detector") if isinstance(block.get("detector"), str) else None
        precision = block.get("precision") if isinstance(block.get("precision"), str) else None
        if precision is None and generation.precision_applies:
            precision = generation.precision
        if pages is None and facts is not None:
            pages = facts.pages
        relative = self._relative(sidecar)
        volume_key = self._relative(cbz)
        record: dict[str, Any] = {
            "sidecar_path": relative,
            "volume_key": volume_key,
            "generation_id": generation.id,
            "generation_name": generation.name,
            "machine": machine,
            "account": account,
            "engine": generation.engine,
            "detector": detector or generation.reported_detector,
            "precision": precision,
            "runner_build": build,
            "pages": pages,
            "failed_pages": failed_pages,
            "archive_size": archive_stamp[0] if archive_stamp else None,
            "archive_mtime_ns": archive_stamp[1] if archive_stamp else None,
        }
        try:
            if relative is not None and volume_key is not None:
                self.database.record_ocr_sidecar(record)
            self.database.log_audit_event(
                action=WRITTEN,
                actor_username=account,
                target_type=TARGET_TYPE,
                target_path=self._target(relative, sidecar),
                details={
                    "generation": generation.name,
                    "generation_id": generation.id,
                    "machine": machine,
                    "engine": record["engine"],
                    "detector": record["detector"],
                    "precision": precision,
                    "pages": pages,
                    "failed_pages": failed_pages,
                    "runner_build": build,
                },
            )
        except Exception:  # noqa: BLE001 - a record is never worth a sidecar
            logger.exception("could not record who wrote %s", sidecar)

    def rejected(
        self,
        *,
        cbz: Path,
        generation: GenerationSpec,
        machine: str,
        account: str | None,
        reason: str,
    ) -> None:
        """A delivered result was refused: audit it, with why. No table row."""
        sidecar = generation.sidecar_paths(cbz)[0]
        try:
            self.database.log_audit_event(
                action=REJECTED,
                actor_username=account,
                target_type=TARGET_TYPE,
                target_path=self._target(self._relative(sidecar), sidecar),
                details={
                    "generation": generation.name,
                    "generation_id": generation.id,
                    "machine": machine,
                    "engine": generation.engine,
                    "reason": reason[:500],
                },
            )
        except Exception:  # noqa: BLE001 - auditing never fails the queue
            logger.exception("could not audit the rejected result for %s", cbz)

    def forget(self, sidecar: Path) -> None:
        """A sidecar left the library some other way (a sweep): drop its row."""
        relative = self._relative(sidecar)
        if relative is None:
            return
        try:
            self.database.forget_ocr_sidecar(relative)
        except Exception:  # noqa: BLE001
            logger.exception("could not forget the record of %s", sidecar)
