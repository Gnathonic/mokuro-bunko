"""Live OCR settings changes: what takes effect at once, what kills the job
that is running, and -- just as load-bearing -- what must leave it alone.

mokuro in fp16 rides along here because its precision mode is the one that
adds an argument to mokuro's command line, and because an fp16 mokuro row is
not exclusive with a plain one: the sidecar is named after the ROW, so the
two may be two rows of one list.
"""

from __future__ import annotations

import json
import threading
import zipfile
from collections.abc import Callable, Sequence
from dataclasses import replace
from pathlib import Path
from typing import Any
from unittest.mock import patch

import pytest

from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.engines import get_engine, uses_mokuro_env
from mokuro_bunko.ocr.generations import (
    GenerationPools,
    GenerationSpec,
    parse_generation_list,
)
from mokuro_bunko.ocr.processor import MokuroRunResult, OCRProcessor
from mokuro_bunko.ocr.watcher import OCRWorker

# The row every list needs: exactly one enabled generation writes the bare
# <Volume>.mokuro, and until it has, no other row may claim the volume.
MOKURO = {"name": "mokuro", "engine": "mokuro", "primary": True}


def _rows(*rows: dict[str, Any]) -> list[GenerationSpec]:
    """Parse a generations list, minting ``g-1``, ``g-2``, ... in order.

    Edits are then made with ``dataclasses.replace`` so the ``id`` survives:
    that is what the admin panel does, and the whole cancel rule is keyed on
    it.
    """
    return parse_generation_list([dict(row) for row in rows])


def _make_cbz(path: Path, pages: int = 2) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w") as zf:
        for i in range(pages):
            zf.writestr(f"{i:03d}.jpg", b"\xff\xd8\xff\xd9")
    return path


def _primary_sidecar(cbz: Path) -> None:
    """Mark the primary row's work done, so the rows below it may queue."""
    (cbz.parent / f"{cbz.stem}.mokuro").write_text(json.dumps({"pages": []}), encoding="utf-8")


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    (tmp_path / "library").mkdir()
    (tmp_path / "inbox").mkdir()
    return tmp_path


class TestMokuroInFp16:
    def test_registry(self) -> None:
        assert get_engine("mokuro").uses_mokuro_env
        assert uses_mokuro_env("mokuro") and not uses_mokuro_env("hayai-nova")
        row = _rows({**MOKURO, "precision": "fp16"})[0]
        assert row.precision_applies and row.precision == "fp16"

    def test_it_may_now_run_beside_plain_mokuro(self) -> None:
        """The pair used to be refused: both engines wrote ``<Volume>.mokuro``.

        The file name belongs to the row now, so an fp16 row beside the
        primary mokuro one simply writes a second, named file.
        """
        rows = _rows(MOKURO, {"name": "fp16", "engine": "mokuro", "precision": "fp16"})
        assert [row.engine for row in rows] == ["mokuro", "mokuro"]
        assert [row.sidecar_suffix for row in rows] == [".mokuro", ".fp16.mokuro"]

    def test_fp16_flag_reaches_the_mokuro_command(self, storage: Path) -> None:
        rows = _rows(MOKURO, {"name": "fp16", "engine": "mokuro", "precision": "fp16"})
        proc = OCRProcessor(storage_path=storage, generations=rows, python_path=Path("/py"))
        seen: dict[str, Any] = {}

        def fake_run(cmd: list[str], *args: Any, **kwargs: Any) -> MokuroRunResult:
            seen["cmd"] = cmd
            seen["generation"] = kwargs.get("generation")
            return MokuroRunResult(True, None, None)

        with patch.object(proc, "_run_ocr_subprocess", side_effect=fake_run):
            proc._run_engine(rows[1], storage / "in", storage / "out")
            assert "--fp16" in seen["cmd"]
            # The row goes with the run: it names the log and decides the
            # OS priority the subprocess starts at.
            assert seen["generation"] is rows[1]
            proc._run_engine(rows[0], storage / "in", storage / "out")
            assert "--fp16" not in seen["cmd"]

    def test_a_primary_fp16_row_claims_the_bare_sidecar(self, storage: Path) -> None:
        # Any engine may be the primary one; what it writes is the bare file.
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        rows = _rows({"name": "fp16", "engine": "mokuro", "primary": True, "precision": "fp16"})
        proc = OCRProcessor(storage_path=storage, generations=rows)
        assert proc.missing_generations(cbz) == rows
        _primary_sidecar(cbz)
        assert proc.missing_generations(cbz) == []


def _worker(storage: Path, generations: Sequence[GenerationSpec]) -> OCRWorker:
    return OCRWorker(
        storage_path=storage,
        poll_interval=30.0,
        generations=generations,
        engines_python_path=Path("/nonexistent"),
        # The per-volume path: a cancel here is a killed subprocess, which
        # is what this file is about. Sessions are cancelled whole and are
        # covered in `test_ocr_sessions.py`.
        sessions=False,
    )


def _apply_mid_job(worker: OCRWorker, new_rows: Sequence[GenerationSpec]) -> list[bool]:
    """Apply ``new_rows`` while a job runs; report the cancels it issued.

    One scan, one job: the OCR subprocess is replaced by a fake that blocks
    until either the settings change kills it or this helper releases it, so
    both outcomes go through the worker's real job bookkeeping.
    """
    cancelled: list[bool] = []
    started = threading.Event()
    release = threading.Event()

    def fake_process(path: Path, generation: GenerationSpec) -> bool:
        started.set()
        release.wait(timeout=5)
        # A killed subprocess reports failure; an untouched run succeeds.
        return not cancelled

    def fake_cancel() -> bool:
        cancelled.append(True)
        release.set()
        return True

    with (
        patch.object(worker.processor, "process_library_ocr", side_effect=fake_process),
        patch.object(worker.processor, "cancel_active", side_effect=fake_cancel),
    ):
        thread = threading.Thread(target=worker._scan_ocr_once)
        thread.start()
        assert started.wait(timeout=5)
        worker.apply_settings(new_rows)
        release.set()
        thread.join(timeout=5)
    assert not thread.is_alive()
    return cancelled


class TestWorkerApplySettings:
    def test_a_removed_row_stops_queueing(self, storage: Path) -> None:
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        _primary_sidecar(cbz)
        rows = _rows(MOKURO, {"engine": "hayai-nova"})
        worker = _worker(storage, rows)
        assert worker._ocr_candidates() == [(cbz, rows[1].id)]

        worker.apply_settings(rows[:1], poll_interval=7)

        assert worker.generations == rows[:1]
        assert worker.processor.generations == rows[:1]
        assert worker.poll_interval == 7.0
        assert worker._ocr_candidates() == []

    def test_an_added_row_joins_the_backlog(self, storage: Path) -> None:
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        _primary_sidecar(cbz)
        rows = _rows(MOKURO, {"engine": "paddle-manga"})
        worker = _worker(storage, rows[:1])
        assert worker._ocr_candidates() == []
        worker.apply_settings(rows)
        assert worker._ocr_candidates() == [(cbz, rows[1].id)]

    def test_a_running_job_of_a_removed_row_is_cancelled_not_failed(self, storage: Path) -> None:
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        _primary_sidecar(cbz)
        rows = _rows(MOKURO, {"engine": "hayai-nova"})
        worker = _worker(storage, rows)

        assert _apply_mid_job(worker, rows[:1]) == [True]
        # The volume did nothing wrong: no failure record, so no backoff and
        # nothing for the queue page to show as broken.
        assert not (storage / ".ocr-failures.json").exists()

    @pytest.mark.parametrize(
        ("edit", "cancels"),
        [
            pytest.param(
                lambda rows: [rows[0], replace(rows[1], enabled=False)], True, id="disabled"
            ),
            pytest.param(
                lambda rows: [rows[0], replace(rows[1], engine="paddle-manga")], True, id="engine"
            ),
            pytest.param(
                lambda rows: [rows[0], replace(rows[1], detector="ctd")],
                True,
                id="detector",
            ),
            pytest.param(
                lambda rows: [rows[0], replace(rows[1], patch_budget=256)],
                True,
                id="patch-budget",
            ),
            pytest.param(
                lambda rows: [rows[0], replace(rows[1], name="renamed")], False, id="renamed"
            ),
            pytest.param(
                lambda rows: [replace(rows[0], primary=False), replace(rows[1], primary=True)],
                False,
                id="made-primary",
            ),
            pytest.param(lambda rows: [rows[1], rows[0]], False, id="moved-up-the-list"),
            pytest.param(
                lambda rows: [
                    rows[0],
                    replace(
                        rows[1],
                        pools=GenerationPools(stage_workers={rows[1].stage_keys[0]: 2}),
                    ),
                ],
                False,
                id="pools",
            ),
        ],
    )
    def test_only_an_output_affecting_edit_kills_the_running_job(
        self,
        storage: Path,
        edit: Callable[[list[GenerationSpec]], list[GenerationSpec]],
        cancels: bool,
    ) -> None:
        """A rename or a retune must not throw away work in progress.

        What the subprocess is writing is decided by its engine, detector and
        patch budget. Everything else only decides where the finished file
        lands or when the row runs next, and the job carries the row it
        started with for exactly that reason; pool sizes provably never
        change the output at all.
        """
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        _primary_sidecar(cbz)
        rows = _rows(MOKURO, {"engine": "hayai-nova"})
        worker = _worker(storage, rows)

        edited = edit(rows)
        # The comparison is by the row's immutable id, which every edit keeps.
        assert {row.id for row in edited} == {row.id for row in rows}
        assert _apply_mid_job(worker, edited) == ([True] if cancels else [])
        # Whatever the reason, a cancel is the operator's doing and not the
        # volume's fault: no failure record, so no exponential backoff.
        assert not (storage / ".ocr-failures.json").exists()


class TestProcessorCancel:
    def test_cancel_kills_the_active_subprocess(self, storage: Path) -> None:
        import subprocess
        import sys

        proc = OCRProcessor(
            storage_path=storage, generations=_rows(MOKURO), python_path=Path(sys.executable)
        )
        cmd = [sys.executable, "-c", "import time; time.sleep(30)"]
        log = storage / "run.log"
        out = storage / "out"
        out.mkdir()
        result: dict[str, MokuroRunResult] = {}

        def run() -> None:
            result["r"] = proc._run_ocr_subprocess(cmd, storage / "in", out, log, label="Mokuro")

        thread = threading.Thread(target=run)
        thread.start()
        for _ in range(100):
            if proc._active_process is not None:
                break
            threading.Event().wait(0.05)
        assert isinstance(proc._active_process, subprocess.Popen)
        assert proc.cancel_active()
        thread.join(timeout=10)
        assert not thread.is_alive()
        assert result["r"].ok is False
        assert "cancelled" in (result["r"].error or "")
        assert proc._active_process is None
        assert proc.cancel_active() is False


class _FakeInstaller:
    """An OCR environment: which detector extras are there, and which were asked for.

    ``present`` is what is installed; ``detectors`` is what the last
    ``set_detectors`` said this environment is for -- the two are separate
    here precisely because conflating them is the bug that method fixes.
    """

    def __init__(self, installed: bool = True, present: set[str] | None = None) -> None:
        self.installed = installed
        self.present = present if present is not None else {"ctd", "ppocr-manga"}
        self.detector = "ppocr-manga"
        self.detectors: tuple[str, ...] = ("ppocr-manga",)
        self.install_calls: list[str] = []

    def is_installed(self) -> bool:
        return self.installed

    def set_detectors(self, detectors: Sequence[str]) -> None:
        self.detectors = tuple(detectors) or (self.detector,)
        self.detector = self.detectors[0]

    def has_detector(self, detector: str | None = None) -> bool:
        return (detector or self.detector) in self.present

    def install_detector(self, detector: str | None = None) -> bool:
        wanted = detector or self.detector
        self.install_calls.append(wanted)
        self.present.add(wanted)
        return True


class _FakeQueue:
    generations: list[GenerationSpec] = []


class TestOcrControl:
    def _control(
        self, storage: Path, generations: Sequence[GenerationSpec]
    ) -> tuple[OcrControl, OCRWorker]:
        worker = _worker(storage, generations)
        control = OcrControl()
        control.worker = worker
        control.queue_api = _FakeQueue()  # type: ignore[assignment]
        control.mokuro_installer = _FakeInstaller()  # type: ignore[assignment]
        control.engines_installer = _FakeInstaller()  # type: ignore[assignment]
        rows = [row.to_dict() for row in generations]
        control.runtime = {"generations": rows, "active_generations": rows}
        return control, worker

    def test_no_worker_means_restart(self) -> None:
        control = OcrControl()
        out = control.apply(_rows(MOKURO))
        assert out["restart_required"] and not out["applied"]

    def test_applies_when_environments_are_ready(self, storage: Path) -> None:
        rows = _rows(MOKURO, {"engine": "hayai-nova"})
        control, worker = self._control(storage, rows)

        out = control.apply(rows[:1], poll_interval=5)

        assert out == {
            "applied": True,
            "installing": False,
            "restart_required": False,
            "reason": "",
        }
        assert worker.generations == rows[:1]
        assert worker.poll_interval == 5.0
        assert control.queue_api.generations == rows[:1]  # type: ignore[union-attr]
        assert control.runtime == {
            "generations": [rows[0].to_dict()],
            "active_generations": [rows[0].to_dict()],
            # The mokuro row detects behind its own CLI, so the engines
            # environment is now for nothing at all.
            "detectors": [],
            "detector_ready": True,
        }

    def test_a_patch_budget_change_applies_live_without_an_install(self, storage: Path) -> None:
        """It is an argument to a model already on disk, so it needs nothing
        installed and takes effect from the next job."""
        rows = _rows(MOKURO, {"engine": "hayai-nova"})
        control, worker = self._control(storage, rows)
        installer = control.engines_installer

        out = control.apply([rows[0], replace(rows[1], patch_budget=256)])

        assert out["applied"] and not out["installing"]
        assert [row.patch_budget for row in worker.processor.generations] == [512, 256]
        assert installer.install_calls == []  # type: ignore[union-attr]

    def test_missing_engines_env_applies_and_this_server_waits_for_a_restart(
        self, storage: Path
    ) -> None:
        """B10: the environment is missing HERE, which is this server's gap
        and not the queue's -- the row is applied (a processor may run it)
        and only this server's slots leave it until a restart installs it."""
        rows = _rows(MOKURO, {"engine": "hayai-nova"})
        control, worker = self._control(storage, rows[:1])
        control.engines_installer = _FakeInstaller(installed=False)  # type: ignore[assignment]

        out = control.apply(rows)

        assert out["applied"] and out["restart_required"]
        assert "hayai-nova" in out["reason"] and "restart" in out["reason"]
        assert worker.generations == rows
        assert "engines" in worker.local_unavailable

    def test_missing_detector_extras_install_in_background(self, storage: Path) -> None:
        rows = _rows(MOKURO, {"engine": "hayai-nova"})
        control, worker = self._control(storage, rows)
        installer = _FakeInstaller(present={"ppocr-manga"})
        control.engines_installer = installer  # type: ignore[assignment]

        out = control.apply([rows[0], replace(rows[1], detector="ctd")])

        assert out["installing"] and not out["restart_required"]
        # Not yet: the installer describes the LIVE configuration, and these
        # settings are not live until the install has succeeded.
        assert installer.detectors == ("ppocr-manga",)
        assert control._install_thread is not None
        control._install_thread.join(timeout=5)
        assert installer.install_calls == ["ctd"]
        # Now both halves have moved together: a readiness check left on the
        # detector from before the change reported a ready environment that
        # was not.
        assert installer.detectors == ("ctd",)
        assert [row.detector for row in worker.generations] == [None, "ctd"]
        assert control.runtime is not None and control.runtime["detectors"] == ["ctd"]

    def test_an_engine_with_its_own_detector_installs_that_one(self, storage: Path) -> None:
        # A ppocr-manga row never uses the detector anything else configured:
        # what it needs is its own detector's onnxruntime.
        rows = _rows(MOKURO, {"engine": "ppocr-manga"})
        control, worker = self._control(storage, rows[:1])
        installer = _FakeInstaller(present=set())
        control.engines_installer = installer  # type: ignore[assignment]

        out = control.apply(rows)

        assert out["installing"] and "ppocr-manga" in out["reason"]
        assert control._install_thread is not None
        control._install_thread.join(timeout=5)
        assert installer.install_calls == ["ppocr-manga"]
        assert [row.engine for row in worker.generations] == ["mokuro", "ppocr-manga"]
