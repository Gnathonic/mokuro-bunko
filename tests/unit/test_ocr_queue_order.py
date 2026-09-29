"""The OCR worker's queue: what runs next, and that the queue page shows
exactly that (one list, computed once, by the scheduler).

The unit of work is a (volume, GENERATION) pair, and the order of the rows in
``ocr.generations`` is the whole of the priority rule: there is no speed
ranking and no hardware ever re-ranks anything.
"""

from __future__ import annotations

import io
import json
import threading
import time
import zipfile
from collections.abc import Callable, Sequence
from pathlib import Path
from typing import Any
from unittest.mock import patch

import pytest

from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.watcher import OCRWorker
from mokuro_bunko.queue.api import QueueAPI


def _make_cbz(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w") as zf:
        zf.writestr("page_000.jpg", b"fake image data")
    return path


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    (tmp_path / "library").mkdir()
    (tmp_path / "inbox").mkdir()
    return tmp_path


def _row(engine: str, *, name: str | None = None, primary: bool = False, **fields: Any) -> dict:
    """One ``ocr.generations`` row, named after its engine unless told otherwise."""
    return {"engine": engine, "name": name or engine, "primary": primary, **fields}


def _generations(*rows: dict[str, Any]) -> list[GenerationSpec]:
    """Parse rows exactly as a configured ``ocr.generations`` would be."""
    return parse_generation_list(list(rows))


_MOKURO_ONLY = _generations(_row("mokuro", primary=True))


def _worker(storage: Path, generations: Sequence[GenerationSpec]) -> OCRWorker:
    return OCRWorker(
        storage_path=storage,
        poll_interval=30.0,
        generations=generations,
        engines_python_path=Path("/nonexistent"),
        # The per-volume path: these tests stand in for a whole OCR run by
        # patching `process_library_ocr`, which a session never calls.
        sessions=False,
    )


def _library(storage: Path, **series: list[str]) -> None:
    for name, volumes in series.items():
        for volume in volumes:
            _make_cbz(storage / "library" / name / f"{volume}.cbz")


def _primary_done(storage: Path, **series: list[str]) -> None:
    """Give these volumes the primary row's sidecar.

    A volume that still owes the enabled primary row its ``<Volume>.mokuro``
    offers ONLY that row to the queue, because every other row's sidecar
    inherits the uuid that file carries. Tests that want several rows pending
    on one volume at once therefore start from a volume that has it.
    """
    for name, volumes in series.items():
        for volume in volumes:
            (storage / "library" / name / f"{volume}.mokuro").write_text("{}", encoding="utf-8")


def _triples(worker: OCRWorker) -> list[tuple[str, str, str]]:
    return [(j["series"], j["volume"], j["generation"]) for j in worker.pending_jobs(max_age=0)]


# What a queue entry IS, for the tests in this file -- which are about the
# ORDER of the queue. Entries also carry a prediction (`pages`, `eta_at`,
# `rate_source`, `reason`, ...) that depends on measured rates and on a wall
# clock; asserting the whole dict here would make every order test a test of
# the ETA as well, and `tests/unit/test_ocr_eta.py` is where that belongs.
_IDENTITY_KEYS = ("series", "volume", "generation", "engine", "detector", "attempts")


def _identity(entries: Sequence[dict[str, Any]]) -> list[dict[str, Any]]:
    return [{key: entry[key] for key in _IDENTITY_KEYS if key in entry} for entry in entries]


def _sidecar(path: Path, generation: GenerationSpec) -> Path:
    return generation.sidecar_paths(path)[0]


def _run_scan(
    worker: OCRWorker, fail: set[tuple[str, str]] | None = None
) -> tuple[list[tuple[str, str, str]], list[list[tuple[str, str, str]]]]:
    """Run one scan with a fake processor.

    Returns the jobs in the order they ran, and what the queue page would
    have listed as pending WHILE each of them ran.
    """
    ran: list[tuple[str, str, str]] = []
    shown: list[list[tuple[str, str, str]]] = []

    def fake_process(path: Path, generation: GenerationSpec) -> bool:
        ran.append((path.parent.name, path.stem, generation.name))
        shown.append(_triples(worker))
        if fail and (path.stem, generation.name) in fail:
            return False
        _sidecar(path, generation).write_text("{}", encoding="utf-8")
        return True

    worker._running = True
    with patch.object(worker.processor, "process_library_ocr", side_effect=fake_process):
        worker._scan_ocr_once()
    return ran, shown


class TestProcessingOrder:
    def test_row_order_decides_the_queue(self, storage: Path) -> None:
        # There is no engine speed ranking any more: rearranging the rows IS
        # how the queue is prioritised, and nothing else has a vote.
        _library(storage, S=["A", "B"])
        _primary_done(storage, S=["A", "B"])
        rows = _generations(
            _row("mokuro", primary=True), _row("paddle-manga"), _row("ppocr-manga")
        )
        worker = _worker(storage, rows)
        assert [g for _, _, g in _triples(worker)] == (
            ["paddle-manga"] * 2 + ["ppocr-manga"] * 2
        )

        # The same rows, the last one moved to the top: the queue follows.
        worker.apply_settings([rows[2], rows[0], rows[1]])
        assert [g for _, _, g in _triples(worker)] == (
            ["ppocr-manga"] * 2 + ["paddle-manga"] * 2
        )

    def test_a_disabled_row_is_not_queued(self, storage: Path) -> None:
        _library(storage, S=["A"])
        _primary_done(storage, S=["A"])
        rows = _generations(
            _row("mokuro", primary=True),
            _row("paddle-manga", enabled=False),
            _row("ppocr-manga"),
        )
        worker = _worker(storage, rows)
        assert _triples(worker) == [("S", "A", "ppocr-manga")]

    def test_round_robin_by_series_in_reading_order(self, storage: Path) -> None:
        _library(
            storage,
            Alpha=["Volume 1", "Volume 2", "Volume 10"],
            Beta=["Volume 1"],
            Gamma=["Volume 1", "Volume 2"],
        )
        worker = _worker(storage, _MOKURO_ONLY)
        assert [(s, v) for s, v, _ in _triples(worker)] == [
            ("Alpha", "Volume 1"),
            ("Beta", "Volume 1"),
            ("Gamma", "Volume 1"),
            ("Alpha", "Volume 2"),
            ("Gamma", "Volume 2"),
            ("Alpha", "Volume 10"),
        ]

    def test_creation_time_no_longer_matters(self, storage: Path) -> None:
        import os

        _library(storage, S=["Volume 1", "Volume 2"])
        now = time.time()
        os.utime(storage / "library" / "S" / "Volume 2.cbz", (now - 500, now - 500))
        worker = _worker(storage, _MOKURO_ONLY)
        assert [v for _, v, _ in _triples(worker)] == ["Volume 1", "Volume 2"]

    def test_the_scan_runs_exactly_the_list_that_was_shown(self, storage: Path) -> None:
        _library(storage, Alpha=["1", "2", "3"], Beta=["1"], Gamma=["1", "2"])
        _primary_done(storage, Alpha=["1", "2", "3"], Beta=["1"], Gamma=["1", "2"])
        worker = _worker(
            storage,
            _generations(
                _row("mokuro", primary=True), _row("ppocr-manga"), _row("paddle-manga")
            ),
        )
        projected = _triples(worker)
        assert len(projected) == 12

        ran, shown = _run_scan(worker)

        assert ran == projected
        # While job N runs the page lists jobs N+1.. in the same order: the
        # running job is not repeated and nothing is reshuffled.
        for index, listed in enumerate(shown):
            assert listed == projected[index + 1 :]
        assert _triples(worker) == []

    def test_a_failed_job_in_backoff_does_not_block_the_round(self, storage: Path) -> None:
        _library(storage, Alpha=["1", "2"], Beta=["1"])
        worker = _worker(storage, _MOKURO_ONLY)

        ran, _ = _run_scan(worker, fail={("1", "mokuro")})
        # Alpha 1 and Beta 1 both fail; Alpha 2 still gets its turn, once.
        assert ran == [("Alpha", "1", "mokuro"), ("Beta", "1", "mokuro"), ("Alpha", "2", "mokuro")]
        # Both failures are in backoff: not pending, and they hold no slot.
        assert _triples(worker) == []
        _make_cbz(storage / "library" / "Alpha" / "3.cbz")
        assert _triples(worker) == [("Alpha", "3", "mokuro")]

    def test_a_retry_that_is_due_is_listed_with_its_attempts(self, storage: Path) -> None:
        _library(storage, S=["A", "B"])
        worker = _worker(storage, _MOKURO_ONLY)
        worker._record_ocr_failure(storage / "library" / "S" / "A.cbz", _MOKURO_ONLY[0])
        assert _triples(worker) == [("S", "B", "mokuro")]
        failures = json.loads((storage / ".ocr-failures.json").read_text(encoding="utf-8"))
        failures["S/A.cbz"]["last_attempt_at"] = time.time() - 3600
        # Keep the archive older than the attempt, or the record is reset.
        import os

        os.utime(storage / "library" / "S" / "A.cbz", (time.time() - 7200, time.time() - 7200))
        (storage / ".ocr-failures.json").write_text(json.dumps(failures), encoding="utf-8")
        assert _identity(worker.pending_jobs(max_age=0)) == [
            {
                "series": "S",
                "volume": "A",
                "generation": "mokuro",
                "engine": "mokuro",
                "detector": None,
                "attempts": 1,
            },
            {
                "series": "S",
                "volume": "B",
                "generation": "mokuro",
                "engine": "mokuro",
                "detector": None,
            },
        ]

    def test_new_volume_gets_the_first_row_before_the_backlog_resumes(
        self, storage: Path
    ) -> None:
        _library(storage, Alpha=["1"], Beta=["1"])
        rows = _generations(_row("mokuro", primary=True), _row("hayai-nova"))
        worker = _worker(storage, rows)
        ran: list[tuple[str, str, str]] = []

        def fake_process(path: Path, generation: GenerationSpec) -> bool:
            ran.append((path.parent.name, path.stem, generation.name))
            _sidecar(path, generation).write_text("{}", encoding="utf-8")
            if (path.parent.name, generation.name) == ("Alpha", "hayai-nova"):
                _make_cbz(storage / "library" / "Gamma" / "1.cbz")
            return True

        worker._running = True
        with patch.object(worker.processor, "process_library_ocr", side_effect=fake_process):
            worker._scan_ocr_once()
        assert ran == [
            ("Alpha", "1", "mokuro"),
            ("Beta", "1", "mokuro"),
            ("Alpha", "1", "hayai-nova"),
            ("Gamma", "1", "mokuro"),  # arrived during the slow backlog
            ("Beta", "1", "hayai-nova"),
            ("Gamma", "1", "hayai-nova"),
        ]

    def test_pending_list_is_cached_between_polls(self, storage: Path) -> None:
        _library(storage, S=["A"])
        worker = _worker(storage, _MOKURO_ONLY)
        assert len(worker.pending_jobs()) == 1
        _make_cbz(storage / "library" / "S" / "B.cbz")
        # The page polls every few seconds; the library is not rescanned each time.
        assert len(worker.pending_jobs(max_age=60.0)) == 1
        assert len(worker.pending_jobs(max_age=0)) == 2

    def test_concurrent_polls_on_a_cold_cache_share_one_computation(self, storage: Path) -> None:
        # Every computation walks the library; eight pages opened at once
        # must not start eight walks.
        _library(storage, S=["A", "B"])
        worker = _worker(storage, _MOKURO_ONLY)
        compute = worker._upcoming_ocr_jobs
        calls: list[int] = []
        entered = threading.Event()
        release = threading.Event()

        def slow_compute(**kwargs: Any) -> list[tuple[Path, str]]:
            calls.append(1)
            entered.set()
            assert release.wait(10)
            return compute(**kwargs)

        results: list[list[dict[str, Any]]] = []
        with patch.object(worker, "_upcoming_ocr_jobs", side_effect=slow_compute):
            polls = [
                threading.Thread(target=lambda: results.append(worker.pending_jobs()))
                for _ in range(8)
            ]
            for poll in polls:
                poll.start()
            assert entered.wait(10)
            time.sleep(0.3)  # let the other seven reach the point where they wait
            release.set()
            for poll in polls:
                poll.join(10)

        assert len(calls) == 1
        assert len(results) == 8
        assert all(result == results[0] and len(result) == 2 for result in results)
        # Each caller owns its copy: the page code may not corrupt the cache.
        results[0][0]["series"] = "changed"
        assert worker.pending_jobs()[0]["series"] == "S"

    def test_a_queue_change_during_the_computation_is_not_served_from_it(
        self, storage: Path
    ) -> None:
        _library(storage, S=["A"])
        worker = _worker(storage, _MOKURO_ONLY)
        compute = worker._upcoming_ocr_jobs

        def compute_then_queue_changes(**kwargs: Any) -> list[tuple[Path, str]]:
            jobs = compute(**kwargs)
            with worker._lock:
                worker._queue_generation += 1  # a job started meanwhile
            return jobs

        with patch.object(worker, "_upcoming_ocr_jobs", side_effect=compute_then_queue_changes):
            worker.pending_jobs()
        with patch.object(worker, "_upcoming_ocr_jobs", wraps=compute) as recomputed:
            worker.pending_jobs(max_age=60.0)
        assert recomputed.call_count == 1

    def _replaced_after_failure(self, storage: Path, worker: OCRWorker) -> Path:
        """A failure record whose archive was replaced since (mtime is newer)."""
        path = storage / "library" / "S" / "A.cbz"
        worker._record_ocr_failure(path, _MOKURO_ONLY[0])
        failures_file = storage / ".ocr-failures.json"
        failures = json.loads(failures_file.read_text(encoding="utf-8"))
        failures["S/A.cbz"]["last_attempt_at"] = time.time() - 60
        failures["S/A.cbz"]["attempts"] = 3
        failures_file.write_text(json.dumps(failures), encoding="utf-8")
        return failures_file

    def test_the_queue_page_never_writes_failure_records(self, storage: Path) -> None:
        # Resetting the record of a replaced archive is the worker's write.
        # The page only reads: it lists the volume as a fresh job and leaves
        # the file exactly as it was.
        _library(storage, S=["A"])
        worker = _worker(storage, _MOKURO_ONLY)
        failures_file = self._replaced_after_failure(storage, worker)
        before = failures_file.read_bytes()

        with patch.object(worker, "_save_failures", side_effect=AssertionError("page wrote")):
            listed = worker.pending_jobs(max_age=0)
            via_control = OcrControl()
            via_control.worker = worker
            app = QueueAPI(
                _dummy_app, storage_base_path=str(storage), generations=_MOKURO_ONLY,
                ocr_control=via_control,
            )  # fmt: skip
            shown = _status(app)["pending_ocr"]

        # Replaced since the failure = a fresh job: no "attempts" on it.
        assert (
            _identity(listed)
            == _identity(shown)
            == [
                {
                    "series": "S",
                    "volume": "A",
                    "generation": "mokuro",
                    "engine": "mokuro",
                    "detector": None,
                }
            ]
        )
        assert failures_file.read_bytes() == before

    def test_the_workers_scan_resets_the_record_of_a_replaced_archive(self, storage: Path) -> None:
        _library(storage, S=["A", "B"])
        worker = _worker(storage, _MOKURO_ONLY)
        failures_file = self._replaced_after_failure(storage, worker)

        ran, _ = _run_scan(worker, fail={("A", "mokuro")})

        assert ran == [("S", "A", "mokuro"), ("S", "B", "mokuro")]
        failures = json.loads(failures_file.read_text(encoding="utf-8"))
        # Started fresh: attempt 1 again, not attempt 4.
        assert failures["S/A.cbz"]["attempts"] == 1

    def test_log_line_names_the_real_generation_order(self, storage: Path) -> None:
        _library(storage, S=["A"])
        messages: list[str] = []
        worker = OCRWorker(
            storage_path=storage,
            generations=_generations(
                _row("paddle-manga", primary=True), _row("ppocr-manga")
            ),
            engines_python_path=Path("/nonexistent"),
            status_callback=messages.append,
            sessions=False,
        )
        with patch.object(worker.processor, "process_library_ocr", return_value=False):
            worker._scan_ocr_once()
        found = next(m for m in messages if m.startswith("Found"))
        # The list order, named as such: no ranking is claimed, because none
        # is applied.
        assert "generations, in run order: paddle-manga, ppocr-manga" in found
        assert "fastest" not in found
        assert "priority order" not in found


class _Resp:
    def __init__(self) -> None:
        self.status = ""

    def start_response(
        self, status: str, headers: list[tuple[str, str]], exc_info: Any = None
    ) -> Callable[[bytes], None]:
        self.status = status
        return lambda data: None


def _status(app: Callable[..., Any]) -> dict[str, Any]:
    resp = _Resp()
    environ = {
        "REQUEST_METHOD": "GET",
        "PATH_INFO": "/queue/api/status",
        "QUERY_STRING": "",
        "wsgi.input": io.BytesIO(b""),
        "wsgi.errors": io.StringIO(),
        "wsgi.url_scheme": "http",
        "SERVER_NAME": "localhost",
        "SERVER_PORT": "8080",
    }
    b"".join(app(environ, resp.start_response))
    assert resp.status.startswith("200"), resp.status
    # The endpoint sends a shaped, per-level payload (`queue.shape`); these
    # tests are about the model underneath it, which `raw_status` returns.
    return app.raw_status()  # type: ignore[attr-defined, no-any-return]


def _dummy_app(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
    start_response("404 Not Found", [])
    return [b""]


def _api_triples(data: dict[str, Any]) -> list[tuple[str, str, str]]:
    return [(p["series"], p["volume"], p["generation"]) for p in data["pending_ocr"]]


class TestQueueApiOrder:
    def _app(
        self, storage: Path, worker: OCRWorker | None, generations: Sequence[GenerationSpec]
    ) -> QueueAPI:
        control = OcrControl()
        control.worker = worker
        return QueueAPI(
            _dummy_app,
            storage_base_path=str(storage),
            generations=generations,
            ocr_control=control,
        )

    def test_status_returns_the_schedulers_list(self, storage: Path) -> None:
        _library(storage, Alpha=["Volume 1", "Volume 2", "Volume 10"], Beta=["Volume 1"])
        _primary_done(storage, Alpha=["Volume 1", "Volume 2", "Volume 10"], Beta=["Volume 1"])
        rows = _generations(
            _row("mokuro", primary=True), _row("paddle-manga"), _row("ppocr-manga")
        )
        worker = _worker(storage, rows)
        data = _status(self._app(storage, worker, rows))

        assert _identity(data["pending_ocr"]) == _identity(worker.pending_jobs())
        # `generations` replaces the old `engines`/`engine_order` pair, and
        # carries what each row runs as well as what it is called.
        assert "engine_order" not in data
        assert data["generations"] == [
            {"id": rows[0].id, "name": "mokuro", "engine": "mokuro", "detector": None},
            {
                "id": rows[1].id,
                "name": "paddle-manga",
                "engine": "paddle-manga",
                "detector": "ppocr-manga",
            },
            {
                "id": rows[2].id,
                "name": "ppocr-manga",
                "engine": "ppocr-manga",
                "detector": "ppocr-manga",
            },
        ]
        assert _api_triples(data)[:4] == [
            ("Alpha", "Volume 1", "paddle-manga"),
            ("Beta", "Volume 1", "paddle-manga"),
            ("Alpha", "Volume 2", "paddle-manga"),
            ("Alpha", "Volume 10", "paddle-manga"),
        ]

    def test_status_follows_the_worker_through_a_scan(self, storage: Path) -> None:
        _library(storage, Alpha=["1", "2"], Beta=["1"])
        worker = _worker(storage, _MOKURO_ONLY)
        app = self._app(storage, worker, _MOKURO_ONLY)
        projected = _api_triples(_status(app))
        seen: list[tuple[dict[str, Any] | None, list[tuple[str, str, str]]]] = []

        def fake_process(path: Path, generation: GenerationSpec) -> bool:
            data = _status(app)
            seen.append((data["current"], _api_triples(data)))
            _sidecar(path, generation).write_text("{}", encoding="utf-8")
            return True

        worker._running = True
        with patch.object(worker.processor, "process_library_ocr", side_effect=fake_process):
            worker._scan_ocr_once()

        for index, (current, pending) in enumerate(seen):
            assert current is not None
            running = (current["series"], current["volume"], current["generation"])
            # The running job heads the queue (its own card) and is not
            # listed a second time; the rest follows in the projected order.
            assert [running, *pending] == projected[index:]

    def test_the_api_does_not_resort_what_the_worker_returns(self, storage: Path) -> None:
        worker = _worker(storage, _MOKURO_ONLY)
        handed_over = [
            {"series": "Z", "volume": "9", "generation": "mokuro", "engine": "mokuro"},
            {"series": "A", "volume": "1", "generation": "paddle-manga", "engine": "paddle-manga"},
            {"series": "M", "volume": "5", "generation": "mokuro", "engine": "mokuro"},
        ]
        with patch.object(worker, "pending_jobs", return_value=handed_over):
            data = _status(self._app(storage, worker, _MOKURO_ONLY))
        assert _identity(data["pending_ocr"]) == _identity(handed_over)

    def test_without_an_ocr_worker_the_index_list_uses_the_same_rule(self, storage: Path) -> None:
        # OCR disabled (or a cover-only worker): nothing is scheduled, the
        # page lists what is missing from the library index, by the same
        # rules the scheduler claims by -- the rows in their configured
        # order, a volume that still owes the primary its file (Volume 10
        # here) owing its other rows all the same.
        _library(storage, Alpha=["Volume 2", "Volume 10"], Beta=["Volume 1"])
        _primary_done(storage, Alpha=["Volume 2"], Beta=["Volume 1"])
        rows = _generations(_row("mokuro", primary=True), _row("hayai-nova"))
        covers_only = OCRWorker(storage_path=storage, thumbnails_only=True)
        for worker in (None, covers_only):
            data = _status(self._app(storage, worker, rows))
            assert _api_triples(data) == [
                ("Alpha", "Volume 10", "mokuro"),
                ("Alpha", "Volume 2", "hayai-nova"),
                ("Beta", "Volume 1", "hayai-nova"),
                ("Alpha", "Volume 10", "hayai-nova"),
            ]
