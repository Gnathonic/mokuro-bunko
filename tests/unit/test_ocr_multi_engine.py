"""Unit tests for multi-GENERATION OCR: processor sidecar detection, runner
command construction, metadata normalization, and worker candidate pairs.

A generation is one row of ``ocr.generations``: a named recipe of an engine, a
detector and a patch budget. Several rows run over the same volume and two
rows may share an engine, so everything a run is filed under -- the sidecar's
name, the log, the failure record, the workspace cache -- comes from the ROW
and never from its engine.
"""

from __future__ import annotations

import json
import zipfile
from collections.abc import Sequence
from pathlib import Path
from typing import Any
from unittest.mock import patch

import pytest

from mokuro_bunko.ocr.generations import (
    GenerationConfigError,
    GenerationSpec,
    parse_generation_list,
)
from mokuro_bunko.ocr.pipeline_stats import pipeline_stats_path
from mokuro_bunko.ocr.processor import MokuroRunResult, OCRProcessor
from mokuro_bunko.ocr.watcher import OCRWorker


def _make_cbz(path: Path, pages: int = 2) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w") as zf:
        for i in range(pages):
            zf.writestr(f"page_{i:03d}.jpg", b"fake image data")
    return path


def _row(engine: str, *, name: str | None = None, primary: bool = False, **fields: Any) -> dict:
    """One ``ocr.generations`` row, named after its engine unless told otherwise.

    Every row is named explicitly so the sidecar file names these tests assert
    on stay readable: a row that leaves ``name`` out is seeded ``<engine>`` or,
    when it chooses its own detector, ``<engine>-<detector>``.
    """
    return {"engine": engine, "name": name or engine, "primary": primary, **fields}


def _generations(*rows: dict[str, Any]) -> list[GenerationSpec]:
    """Parse rows exactly as a configured ``ocr.generations`` would be."""
    return parse_generation_list(list(rows))


def _named(rows: Sequence[GenerationSpec]) -> dict[str, GenerationSpec]:
    return {row.name: row for row in rows}


def _names(rows: Sequence[GenerationSpec]) -> list[str]:
    return [row.name for row in rows]


def _worker(storage: Path, rows: Sequence[GenerationSpec]) -> OCRWorker:
    return OCRWorker(
        storage_path=storage,
        poll_interval=30.0,
        generations=rows,
        engines_python_path=Path("/nonexistent"),
        # The per-volume path: these tests stand in for a whole OCR run by
        # patching `process_library_ocr`, which a session never calls.
        sessions=False,
    )


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    (tmp_path / "library").mkdir()
    (tmp_path / "inbox").mkdir()
    return tmp_path


class TestSidecarDetection:
    def test_missing_generations_in_configured_order(self, storage: Path) -> None:
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        rows = _generations(
            _row("hayai-nova"),
            _row("mokuro", primary=True),
            _row("paddle-manga"),
        )
        proc = OCRProcessor(storage_path=storage, generations=rows)
        # Every row the volume lacks, in the configured order and in no other:
        # no row waits for the primary's file.
        assert _names(proc.missing_generations(cbz)) == [
            "hayai-nova", "mokuro", "paddle-manga",
        ]
        (storage / "library" / "S" / "V.mokuro").write_text("{}", encoding="utf-8")
        assert _names(proc.missing_generations(cbz)) == ["hayai-nova", "paddle-manga"]
        (storage / "library" / "S" / "V.hayai-nova.mokuro.gz").write_bytes(b"x")
        assert _names(proc.missing_generations(cbz)) == ["paddle-manga"]
        assert proc.needs_sidecar(cbz, _named(rows)["paddle-manga"])
        assert not proc.needs_sidecar(cbz, _named(rows)["hayai-nova"])

    def test_two_rows_on_one_engine_are_separate_work(self, storage: Path) -> None:
        """Rows are the unit, not engines: one row's file never claims another's."""
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        (storage / "library" / "S" / "V.mokuro").write_text("{}", encoding="utf-8")
        rows = _generations(
            _row("mokuro", primary=True),
            _row("hayai-nova", name="nova-ppocr"),
            _row("hayai-nova", name="nova-ctd", detector="ctd"),
        )
        proc = OCRProcessor(storage_path=storage, generations=rows)
        assert _names(proc.missing_generations(cbz)) == ["nova-ppocr", "nova-ctd"]
        (storage / "library" / "S" / "V.nova-ppocr.mokuro").write_text("{}", encoding="utf-8")
        assert _names(proc.missing_generations(cbz)) == ["nova-ctd"]

    def test_secondary_sidecar_does_not_satisfy_the_primary_row(self, storage: Path) -> None:
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        proc = OCRProcessor(storage_path=storage)
        (storage / "library" / "S" / "V.hayai-nova.mokuro").write_text("{}", encoding="utf-8")
        assert proc.needs_mokuro_sidecar(cbz)

    def test_default_generations_is_one_primary_mokuro_row(self, storage: Path) -> None:
        rows = OCRProcessor(storage_path=storage).generations
        assert [(row.name, row.engine, row.primary) for row in rows] == [
            ("mokuro", "mokuro", True)
        ]

    def test_unknown_engine_rejected(self, storage: Path) -> None:
        # Parsed, never trusted: the processor builds its rows through this
        # same parser, so an unknown engine cannot reach a run -- and the
        # error says which row and which field to fix.
        with pytest.raises(GenerationConfigError) as caught:
            _generations(_row("tesseract", primary=True))
        assert (caught.value.row, caught.value.field) == (0, "engine")


class TestRunnerCommand:
    def test_command_shape_and_runner_copy(self, storage: Path, tmp_path: Path) -> None:
        fake_python = tmp_path / "engines-python"
        fake_python.write_text("")
        rows = _generations(_row("mokuro", primary=True), _row("hayai-nova"))
        hayai = _named(rows)["hayai-nova"]
        proc = OCRProcessor(
            storage_path=storage, generations=rows, engines_python_path=fake_python
        )
        workspace = storage / ".processing" / "ws"
        extract = workspace / "Vol 01"
        extract.mkdir(parents=True)
        cmd = proc._engine_runner_command(hayai, extract, workspace)
        assert cmd[0] == str(fake_python)
        # Staged ONCE per content hash, in a stable directory, rather than
        # copied into every volume's workspace: a session holds one runner
        # open across many volumes and there is no workspace that outlives
        # them.
        runner = Path(cmd[1])
        assert runner.parent.parent == storage / ".processing"
        assert runner.parent.name.startswith("runner-")
        assert runner.read_text(encoding="utf-8").startswith('"""Standalone OCR engine runner')
        args = dict(zip(cmd[2::2], cmd[3::2], strict=True))
        assert args["--engine"] == "hayai-nova"
        assert args["--detector"] == "ppocr-manga"
        assert (runner.parent / "detectors" / "ppocr_manga.py").is_file()
        assert (runner.parent / "detectors" / "_common.py").is_file()
        assert args["--input"] == str(extract)
        # The ROW names the sidecar; this one is not the primary row, so it
        # writes its own postfix rather than the bare .mokuro.
        assert args["--output"] == str(workspace / "Vol 01.hayai-nova.mokuro")
        # Working directories are keyed by the row's immutable id, so two
        # rows on one engine never share a cache or a stats file.
        assert args["--cache-dir"] == str(workspace / "_ocr" / hayai.id / "Vol 01")
        assert args["--stats-file"] == str(pipeline_stats_path(workspace, hayai.id))
        assert args["--generator"].startswith("mokuro-bunko ")
        assert "--volume-uuid" not in args

    def test_patch_budget_reaches_only_the_engine_that_has_one(
        self, storage: Path, tmp_path: Path
    ) -> None:
        """``--patches`` is passed for ``hayai-nova`` and for nothing else.

        On any other engine's command line it would be a claim about a
        resolution that engine's recognizer never read at.
        """
        fake_python = tmp_path / "engines-python"
        fake_python.write_text("")
        rows = _generations(
            _row("mokuro", primary=True),
            _row("hayai-nova"),
            _row("paddle-manga"),
            _row("ppocr-manga"),
        )
        by_name = _named(rows)
        proc = OCRProcessor(
            storage_path=storage, generations=rows, engines_python_path=fake_python
        )
        workspace = storage / ".processing" / "ws"
        (workspace / "V").mkdir(parents=True)

        cmd = proc._engine_runner_command(by_name["hayai-nova"], workspace / "V", workspace)
        assert cmd[cmd.index("--patches") + 1] == "512"
        assert cmd[cmd.index("--output") + 1].endswith("V.hayai-nova.mokuro")

        for engine in ("paddle-manga", "ppocr-manga"):
            assert "--patches" not in proc._engine_runner_command(
                by_name[engine], workspace / "V", workspace
            )

        # The budget is the ROW's, so it is changed by reconfiguring the row.
        retuned = _generations(
            _row("mokuro", primary=True), _row("hayai-nova", patch_budget=256)
        )
        proc.configure(retuned)
        cmd = proc._engine_runner_command(
            _named(retuned)["hayai-nova"], workspace / "V", workspace
        )
        assert cmd[cmd.index("--patches") + 1] == "256"
        with pytest.raises(GenerationConfigError) as caught:
            _generations(_row("hayai-nova", primary=True, patch_budget=300))
        assert caught.value.field == "patch_budget"

    def test_detector_setting_reaches_the_command(self, storage: Path, tmp_path: Path) -> None:
        fake_python = tmp_path / "engines-python"
        fake_python.write_text("")
        rows = _generations(
            _row("mokuro", primary=True), _row("hayai-nova", detector="ctd")
        )
        proc = OCRProcessor(
            storage_path=storage, generations=rows, engines_python_path=fake_python
        )
        workspace = storage / ".processing" / "ws"
        (workspace / "V").mkdir(parents=True)
        cmd = proc._engine_runner_command(_named(rows)["hayai-nova"], workspace / "V", workspace)
        assert cmd[cmd.index("--detector") + 1] == "ctd"
        with pytest.raises(GenerationConfigError) as caught:
            _generations(_row("hayai-nova", primary=True, detector="nope"))
        assert caught.value.field == "detector"

    def test_the_modules_the_runner_imports_are_staged_beside_it(
        self, storage: Path, tmp_path: Path
    ) -> None:
        """The engines env has no ``mokuro_bunko``, so imports go by path.

        Staging is by content hash and not by row: whatever engine the row
        names, the runner gets every module it or a detector adapter might
        import, because one staged build serves every row. A module that is
        missing only fails once a volume is already running.
        """
        fake_python = tmp_path / "engines-python"
        fake_python.write_text("")
        rows = _generations(_row("mokuro", primary=True), _row("hayai-nova"))
        proc = OCRProcessor(
            storage_path=storage, generations=rows, engines_python_path=fake_python
        )
        workspace = storage / ".processing" / "ws"
        (workspace / "V").mkdir(parents=True)
        cmd = proc._engine_runner_command(_named(rows)["hayai-nova"], workspace / "V", workspace)
        staged = Path(cmd[1]).parent
        # The ppocr-manga engine's own modules and its detector adapter, which
        # finds ppocr.py one directory above itself.
        assert (staged / "ppocr.py").is_file()
        assert (staged / "line_layout.py").is_file()
        assert (staged / "line_reconcile.py").is_file()
        assert (staged / "detectors" / "ppocr_manga.py").is_file()
        # The character-map system was removed outright, so its module must
        # not be dragged along -- a staged build is what the runner can
        # import, and nothing should be able to import this again.
        assert not (staged / "charmap.py").exists()

    def test_volume_uuid_inherited_from_primary_sidecar(
        self, storage: Path, tmp_path: Path
    ) -> None:
        fake_python = tmp_path / "engines-python"
        fake_python.write_text("")
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        (storage / "library" / "S" / "V.mokuro").write_text(
            json.dumps({"volume_uuid": "abc-123"}), encoding="utf-8"
        )
        rows = _generations(_row("mokuro", primary=True), _row("paddle-manga"))
        proc = OCRProcessor(
            storage_path=storage,
            generations=rows,
            engines_python_path=fake_python,
        )
        workspace = storage / ".processing" / "ws"
        (workspace / "V").mkdir(parents=True)
        cmd = proc._engine_runner_command(
            _named(rows)["paddle-manga"], workspace / "V", workspace, source_cbz=cbz
        )
        assert cmd[cmd.index("--volume-uuid") + 1] == "abc-123"

    def test_missing_engines_env_is_a_clean_failure(self, storage: Path) -> None:
        rows = _generations(_row("mokuro", primary=True), _row("hayai-nova"))
        proc = OCRProcessor(storage_path=storage, generations=rows)
        proc.engines_python_path = None
        workspace = storage / ".processing" / "ws"
        (workspace / "V").mkdir(parents=True)
        result = proc._run_engine(_named(rows)["hayai-nova"], workspace / "V", workspace)
        assert not result.ok
        assert "engines environment not installed" in (result.error or "")
        assert "install-ocr --engines hayai-nova" in (result.error or "")

    def test_run_engine_dispatches_a_monolithic_row_to_the_legacy_path(
        self, storage: Path
    ) -> None:
        proc = OCRProcessor(storage_path=storage)
        row = proc.generations[0]
        with patch.object(proc, "_run_mokuro", return_value=MokuroRunResult(True)) as legacy:
            assert proc._run_engine(row, Path("in"), Path("out"), total_images=3).ok
        legacy.assert_called_once_with(
            Path("in"), Path("out"), total_images=3, generation=row, source_cbz=None
        )

    def test_log_path_per_generation(self, storage: Path) -> None:
        rows = _generations(
            _row("mokuro", primary=True),
            _row("hayai-nova", name="nova-ppocr"),
            _row("hayai-nova", name="nova-ctd", detector="ctd"),
        )
        by_name = _named(rows)
        proc = OCRProcessor(storage_path=storage, generations=rows)
        assert proc._get_mokuro_log_path(Path("x/Vol.cbz"), by_name["mokuro"]).name == "Vol.log"
        # Named after the ROW: two rows on one engine must not truncate each
        # other's log while both subprocesses are writing to it.
        assert (
            proc._get_mokuro_log_path(Path("x/Vol.cbz"), by_name["nova-ppocr"]).name
            == "Vol.nova-ppocr.log"
        )
        assert (
            proc._get_mokuro_log_path(Path("x/Vol.cbz"), by_name["nova-ctd"]).name
            == "Vol.nova-ctd.log"
        )

    def test_log_path_names_the_series_of_a_library_volume(self, storage: Path) -> None:
        # Two series both have a "Volume 1.cbz" and both run at once once
        # there is more than one slot; each run opens its log with "w", so
        # one log path for both would mean each shredding the other's.
        rows = _generations(_row("mokuro", primary=True), _row("hayai-nova"))
        by_name = _named(rows)
        proc = OCRProcessor(storage_path=storage, generations=rows)
        alpha = storage / "library" / "Alpha" / "Volume 1.cbz"
        beta = storage / "library" / "Beta" / "Volume 1.cbz"
        workspace_dir = storage / ".processing" / "ws" / "Volume 1"
        first = proc._get_mokuro_log_path(workspace_dir, by_name["mokuro"], alpha)
        second = proc._get_mokuro_log_path(workspace_dir, by_name["mokuro"], beta)
        assert first.name == "Alpha_Volume 1.log"
        assert second.name == "Beta_Volume 1.log"
        assert (
            proc._get_mokuro_log_path(workspace_dir, by_name["hayai-nova"], alpha).name
            == "Alpha_Volume 1.hayai-nova.log"
        )


class TestLibraryOcrPerGeneration:
    def test_generation_sidecar_lands_next_to_cbz_with_shared_uuid(self, storage: Path) -> None:
        cbz = _make_cbz(storage / "library" / "Series A" / "Vol 01.cbz")
        (storage / "library" / "Series A" / "Vol 01.mokuro").write_text(
            json.dumps({"volume_uuid": "primary-uuid"}), encoding="utf-8"
        )
        rows = _generations(
            _row("mokuro", primary=True), _row("hayai-nova"), _row("paddle-manga")
        )
        by_name = _named(rows)
        proc = OCRProcessor(
            storage_path=storage,
            generations=rows,
            engines_python_path=Path("/nonexistent"),
        )
        progress: list[dict] = []
        proc.progress_callback = progress.append

        def fake_run(
            generation: GenerationSpec,
            input_path: Path,
            output_dir: Path,
            total_images: int = 0,
            source_cbz: Path | None = None,
        ) -> MokuroRunResult:
            assert generation.name == "hayai-nova"
            (output_dir / f"{input_path.stem}{generation.sidecar_suffix}").write_text(
                json.dumps(
                    {
                        "version": "0.2.5",
                        "title": "x",
                        "volume": "y",
                        "volume_uuid": "runner-uuid",
                        "pages": [],
                    }
                ),
                encoding="utf-8",
            )
            return MokuroRunResult(True)

        with patch.object(proc, "_run_engine", side_effect=fake_run):
            assert proc.process_library_ocr(cbz, by_name["hayai-nova"])

        out = storage / "library" / "Series A" / "Vol 01.hayai-nova.mokuro"
        data = json.loads(out.read_text(encoding="utf-8"))
        assert data["title"] == "Series A"
        assert data["volume"] == "Vol 01"
        assert data["volume_uuid"] == "primary-uuid"
        assert not proc.needs_sidecar(cbz, by_name["hayai-nova"])
        # ...and only that row's: the next row down is still owed its own file.
        assert proc.needs_sidecar(cbz, by_name["paddle-manga"])
        assert all(p.get("generation") == "hayai-nova" for p in progress)
        assert all(p.get("engine") == "hayai-nova" for p in progress)
        assert progress[-1]["status"] == "done"

    def test_process_library_cbz_runs_every_missing_generation(self, storage: Path) -> None:
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        # The primary row is LAST in the list and must still run first: every
        # other row's sidecar inherits the uuid it writes.
        rows = _generations(_row("hayai-nova"), _row("mokuro", primary=True))
        proc = OCRProcessor(
            storage_path=storage,
            generations=rows,
            engines_python_path=Path("/nonexistent"),
        )
        seen: list[str] = []

        def fake_run(
            generation: GenerationSpec,
            input_path: Path,
            output_dir: Path,
            total_images: int = 0,
            source_cbz: Path | None = None,
        ) -> MokuroRunResult:
            seen.append(generation.name)
            (output_dir / f"{input_path.stem}{generation.sidecar_suffix}").write_text(
                "{}", encoding="utf-8"
            )
            return MokuroRunResult(True)

        with (
            patch.object(proc, "_run_engine", side_effect=fake_run),
            patch.object(proc, "ensure_thumbnail", return_value=True),
        ):
            assert proc.process_library_cbz(cbz)
        assert seen == ["mokuro", "hayai-nova"]
        assert (storage / "library" / "S" / "V.mokuro").exists()
        assert (storage / "library" / "S" / "V.hayai-nova.mokuro").exists()

    def test_inbox_upload_runs_the_primary_generation_only(self, storage: Path) -> None:
        # Exactly one row runs before the volume lands in the library, and it
        # is the PRIMARY one wherever it sits in the list: a secondary row
        # here would write its layer beside an archive with no
        # <Volume>.mokuro to inherit a volume_uuid from, so the runner would
        # stamp a fresh random one and the queue would later write a primary
        # sidecar with a different uuid -- orphaning the layer for good,
        # which nothing downstream can repair. The library loop fills in
        # every other row afterwards.
        cbz = _make_cbz(storage / "inbox" / "New Vol.cbz")
        rows = _generations(_row("hayai-nova"), _row("mokuro", primary=True))
        proc = OCRProcessor(
            storage_path=storage,
            generations=rows,
            engines_python_path=Path("/nonexistent"),
        )
        seen: list[str] = []

        def fake_run(
            generation: GenerationSpec,
            input_path: Path,
            output_dir: Path,
            total_images: int = 0,
            source_cbz: Path | None = None,
        ) -> MokuroRunResult:
            seen.append(generation.name)
            (output_dir / f"{input_path.stem}{generation.sidecar_suffix}").write_text(
                "{}", encoding="utf-8"
            )
            return MokuroRunResult(True)

        with (
            patch.object(proc, "_run_engine", side_effect=fake_run),
            patch.object(proc, "ensure_thumbnail", return_value=True),
        ):
            assert proc.process(cbz)
        assert seen == ["mokuro"]
        assert (storage / "library" / "New Vol.mokuro").exists()
        assert not (storage / "library" / "New Vol.hayai-nova.mokuro").exists()
        assert not cbz.exists()


class TestWorkerPairs:
    def test_candidates_are_generation_major_then_reading_order(self, storage: Path) -> None:
        # Was "..._then_fifo": within a generation the queue used to be FIFO
        # by the archive's creation time. It is now reading order (round-robin
        # across series; see test_ocr_queue_order.py), so B being the OLDER
        # file no longer puts it first.
        import os
        import time

        a = _make_cbz(storage / "library" / "S" / "A.cbz")
        b = _make_cbz(storage / "library" / "S" / "B.cbz")
        # Both volumes already have the primary row's sidecar, so the two
        # secondary rows are claimable at once.
        for volume in (a, b):
            volume.with_suffix(".mokuro").write_text("{}", encoding="utf-8")
        now = time.time()
        os.utime(a, (now - 50, now - 50))
        os.utime(b, (now - 100, now - 100))
        rows = _generations(
            _row("mokuro", primary=True), _row("hayai-nova"), _row("paddle-manga")
        )
        by_name = _named(rows)
        worker = _worker(storage, rows)
        # Every volume gets the row above before any volume gets the row below.
        assert worker._ocr_candidates() == [
            (a, by_name["hayai-nova"].id),
            (b, by_name["hayai-nova"].id),
            (a, by_name["paddle-manga"].id),
            (b, by_name["paddle-manga"].id),
        ]
        for volume in (a, b):
            (volume.parent / f"{volume.stem}.hayai-nova.mokuro").write_text(
                "{}", encoding="utf-8"
            )
        assert worker._ocr_candidates() == [
            (a, by_name["paddle-manga"].id),
            (b, by_name["paddle-manga"].id),
        ]

    def test_a_secondary_row_does_not_wait_for_the_primary_sidecar(
        self, storage: Path
    ) -> None:
        # Every sidecar carries the volume's own uuid whichever lands first,
        # so a layer may be claimed before the bare <Volume>.mokuro exists.
        a = _make_cbz(storage / "library" / "S" / "A.cbz")
        rows = _generations(_row("mokuro", primary=True), _row("hayai-nova"))
        by_name = _named(rows)
        worker = _worker(storage, rows)
        assert worker._ocr_candidates() == [
            (a, by_name["mokuro"].id), (a, by_name["hayai-nova"].id),
        ]
        a.with_suffix(".mokuro").write_text("{}", encoding="utf-8")
        assert worker._ocr_candidates() == [(a, by_name["hayai-nova"].id)]

    def test_scan_picks_up_a_new_volume_before_continuing_the_backlog(self, storage: Path) -> None:
        a = _make_cbz(storage / "library" / "S" / "A.cbz")
        rows = _generations(_row("mokuro", primary=True), _row("hayai-nova"))
        worker = _worker(storage, rows)
        worker._running = True
        calls: list[tuple[str, str]] = []
        new_volume = storage / "library" / "S" / "B.cbz"

        def fake_process(path: Path, generation: GenerationSpec) -> bool:
            calls.append((path.name, generation.name))
            generation.sidecar_paths(path)[0].write_text("{}", encoding="utf-8")
            if path == a and generation.primary:
                _make_cbz(new_volume)  # arrives while A's first row runs
            return True

        with patch.object(worker.processor, "process_library_ocr", side_effect=fake_process):
            worker._scan_ocr_once()
        # B's primary layer jumps ahead of A's second row.
        assert calls == [
            ("A.cbz", "mokuro"),
            ("B.cbz", "mokuro"),
            ("A.cbz", "hayai-nova"),
            ("B.cbz", "hayai-nova"),
        ]

    def test_backlog_generations_run_at_low_priority(self, storage: Path) -> None:
        import sys

        rows = _generations(
            _row("mokuro", primary=True),
            _row("hayai-nova", name="nova-ppocr"),
            _row("hayai-nova", name="nova-ctd", detector="ctd"),
        )
        proc = OCRProcessor(storage_path=storage, generations=rows)
        # The first row of the list keeps normal priority; every row below it
        # is backlog, including one sharing another row's engine.
        assert not proc.is_backlog_generation(rows[0])
        assert proc.is_backlog_generation(rows[1])
        assert proc.is_backlog_generation(rows[2])

        # The first ENABLED row, not the first row.
        with_disabled = _generations(
            _row("paddle-manga", enabled=False), _row("mokuro", primary=True)
        )
        proc.configure(with_disabled)
        assert not proc.is_backlog_generation(with_disabled[1])

        assert proc._priority_popen_kwargs(False) == {}
        low = proc._priority_popen_kwargs(True)
        if sys.platform == "win32":
            assert "creationflags" in low
        else:
            assert callable(low["preexec_fn"])

    def test_failure_records_are_keyed_per_generation(self, storage: Path) -> None:
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        cbz.with_suffix(".mokuro").write_text("{}", encoding="utf-8")
        other = _make_cbz(storage / "library" / "S" / "W.cbz")
        rows = _generations(
            _row("mokuro", primary=True), _row("hayai-nova"), _row("paddle-manga")
        )
        by_name = _named(rows)
        worker = _worker(storage, rows)

        worker._record_ocr_failure(cbz, by_name["hayai-nova"])
        failures = json.loads((storage / ".ocr-failures.json").read_text(encoding="utf-8"))
        assert list(failures) == ["S/V.cbz@hayai-nova"]
        assert failures["S/V.cbz@hayai-nova"]["generation"] == "hayai-nova"
        assert failures["S/V.cbz@hayai-nova"]["engine"] == "hayai-nova"
        # The primary row keeps the legacy bare key so old records stay valid.
        worker._record_ocr_failure(other, by_name["mokuro"])
        failures = json.loads((storage / ".ocr-failures.json").read_text(encoding="utf-8"))
        assert set(failures) == {"S/W.cbz", "S/V.cbz@hayai-nova"}
        # Backoff applies only to the row that failed.
        def of_v() -> list[tuple[Path, str]]:
            return [job for job in worker._ocr_candidates() if job[0] == cbz]

        assert of_v() == [(cbz, by_name["paddle-manga"].id)]
        worker._record_ocr_failure(cbz, by_name["paddle-manga"])
        assert of_v() == []
        worker._clear_ocr_failure(cbz, by_name["paddle-manga"])
        assert of_v() == [(cbz, by_name["paddle-manga"].id)]
        # W's primary is backing off; its layers are not held by it.
        assert [job for job in worker._ocr_candidates() if job[0] == other] == [
            (other, by_name["hayai-nova"].id), (other, by_name["paddle-manga"].id),
        ]

    def test_scan_processes_each_pair(self, storage: Path) -> None:
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        rows = _generations(_row("mokuro", primary=True), _row("hayai-nova"))
        worker = _worker(storage, rows)
        calls: list[tuple[Path, str]] = []

        def fake_process(path: Path, generation: GenerationSpec) -> bool:
            calls.append((path, generation.name))
            generation.sidecar_paths(path)[0].write_text("{}", encoding="utf-8")
            return True

        with patch.object(worker.processor, "process_library_ocr", side_effect=fake_process):
            worker._scan_ocr_once()
        assert calls == [(cbz, "mokuro"), (cbz, "hayai-nova")]
