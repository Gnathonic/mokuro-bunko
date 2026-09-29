"""A benchmark that runs on the machine the user picked (spec sections 4 and 5)."""

from __future__ import annotations

import io
import json
import sys
import time
import zipfile
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr.bench import BenchError, BenchService, bench_path, build_sample
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.remote.library_api import ProcessorAPI, bench_sample_filename
from mokuro_bunko.ocr.remote.profiles import ProcessorProfiles
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry, RemoteBench

ROWS = [
    {"name": "mokuro", "engine": "mokuro", "primary": True},
    {"name": "hayai-ctd", "engine": "hayai-nova", "detector": "ctd"},
]
TOWER_CATALOG: dict[str, Any] = {
    "engines": ["hayai-nova", "mokuro"], "detectors": ["ctd"],
    "devices": [{"id": "gpu:0", "label": "GPU 0 — RTX 4090 (24 GB)"}],
    "serves_mokuro": True,
}


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    library = tmp_path / "library" / "Alpha"
    library.mkdir(parents=True)
    (tmp_path / "inbox").mkdir()
    with zipfile.ZipFile(library / "Volume 1.cbz", "w") as zf:
        for n in range(40):
            zf.writestr(f"page_{n:03d}.jpg", b"fake image data" * 20)
    return tmp_path


def _registry() -> tuple[ProcessorRegistry, Any]:
    registry = ProcessorRegistry(local_name="this server")
    entry = registry.register(
        username="tower", name="tower", host={"gpu": "RTX 4090", "backend": "cuda"},
        catalog=TOWER_CATALOG, max_sessions=1,
    )
    entry.stream_open = True
    return registry, entry


class _FakeWorker:
    """Just enough OCRWorker for BenchService: holds are recorded, not kept."""

    thumbnails_only = False
    local_processing = True

    def __init__(self) -> None:
        self.held: list[str] = []
        self.released: list[str] = []
        self.processor = None

    def preempt_for_bench(
        self, timeout: float = 900.0, processor: str = "local"
    ) -> tuple[bool, list[Any]]:
        del timeout
        self.held.append(processor)
        return True, []

    def release_queue(self, processor: str = "local") -> None:
        self.released.append(processor)


class TestTheBenchOp:
    def test_a_remote_bench_sends_the_sample_as_a_url_it_can_pull(self) -> None:
        _registry_obj, entry = _registry()
        bench = RemoteBench(
            entry, bid="bench-g-2", spec={"engine": "hayai-nova", "detector": "ctd"},
            sample_url="/_processor/p1/bench/bench-g-2/sample", pages=32,
        )
        assert bench.start() is True
        assert entry.ops.get_nowait() == {
            "op": "bench",
            "bid": "bench-g-2",
            "spec": {"engine": "hayai-nova", "detector": "ctd"},
            "sample": "/_processor/p1/bench/bench-g-2/sample",
            "pages": 32,
        }
        assert entry.sessions["bench-g-2"] is bench
        assert entry.open_sessions == 0, "a benchmark is not a session"

    def test_its_events_come_back_through_the_same_frames(self) -> None:
        _registry_obj, entry = _registry()
        bench = RemoteBench(entry, bid="bench-b1", spec={}, sample_url="/x", pages=8)
        bench.start()
        bench.feed({"event": "bench_ready", "pages": 8, "tunable": True}, b"")
        bench.feed({"event": "bench_trial", "n": 1, "pages_per_second": 2.0}, b"")
        bench.feed({"event": "bench_done", "precision": "fp32", "best": {"trial": 1}}, b"")
        assert [bench.poll_event(timeout=1.0)["event"] for _ in range(3)] == [
            "bench_ready", "bench_trial", "bench_done"
        ]

    def test_a_cancel_reaches_the_processor_as_the_one_cancel_op(self) -> None:
        _registry_obj, entry = _registry()
        bench = RemoteBench(entry, bid="bench-b1", spec={}, sample_url="/x", pages=8)
        bench.start()
        entry.ops.get_nowait()
        bench.cancel()
        assert entry.ops.get_nowait() == {"op": "cancel", "bid": "bench-b1"}
        assert "bench-b1" not in entry.sessions

    def test_after_its_terminal_event_it_is_over(self) -> None:
        """A body that closes after `bench_done` is not the processor leaving
        (the events sink drops a processor whose LIVE session's body ends)."""
        _registry_obj, entry = _registry()
        bench = RemoteBench(entry, bid="bench-b1", spec={}, sample_url="/x", pages=8)
        bench.start()
        assert bench.is_alive() is True
        bench.feed({"event": "bench_done", "precision": "fp32", "best": {}}, b"")
        assert bench.is_alive() is False

    def test_a_processor_that_never_opens_the_body_ends_the_bench(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from mokuro_bunko.ocr.remote import session as session_module

        monkeypatch.setattr(session_module, "EVENTS_OPEN_SECONDS", 0.05)
        _registry_obj, entry = _registry()
        bench = RemoteBench(entry, bid="bench-b1", spec={}, sample_url="/x", pages=8)
        bench.start()
        time.sleep(0.1)
        assert bench.poll_event(timeout=0.01) is None
        assert bench.poll_event(timeout=1.0)["event"] == "fatal"
        assert bench.poll_event(timeout=1.0)["event"] == "exit"


class TestChoosingAProcessor:
    @staticmethod
    def _service(
        storage: Path, registry: ProcessorRegistry, worker: Any = None
    ) -> BenchService:
        rows = parse_generation_list([dict(row) for row in ROWS])
        fake = worker or _FakeWorker()
        return BenchService(
            storage,
            worker=lambda: fake,
            generations=lambda: rows,
            processors=registry.entries,
            profiles=ProcessorProfiles(storage),
        )

    def test_an_unknown_processor_is_refused_before_anything_is_held(
        self, storage: Path
    ) -> None:
        registry, _entry = _registry()
        worker = _FakeWorker()
        with pytest.raises(BenchError) as excinfo:
            self._service(storage, registry, worker).enqueue("g-2", None, processor="nowhere")
        assert excinfo.value.status == 400
        assert "nowhere" in excinfo.value.message
        assert worker.held == []

    def test_a_processor_that_cannot_run_the_spec_is_refused(self, storage: Path) -> None:
        registry = ProcessorRegistry(local_name="this server")
        entry = registry.register(
            username="box", name="box", host={},
            catalog={"engines": ["hayai-nova"], "detectors": ["ppocr-manga"],
                     "devices": [], "serves_mokuro": False},
            max_sessions=1,
        )
        entry.stream_open = True
        with pytest.raises(BenchError) as excinfo:
            self._service(storage, registry).enqueue("g-2", None, processor="box")
        assert "ctd" in excinfo.value.message

    def test_a_server_that_runs_no_ocr_is_not_benchmarked_locally(
        self, storage: Path
    ) -> None:
        registry, _entry = _registry()
        worker = _FakeWorker()
        worker.local_processing = False
        with pytest.raises(BenchError) as excinfo:
            self._service(storage, registry, worker).enqueue("g-2", None)
        assert "processor" in excinfo.value.message

    def test_the_run_carries_the_machine_it_will_measure(self, storage: Path) -> None:
        """`enqueue` answers BEFORE the worker thread fills `host`, so the
        machine is read off the run, not off that reply."""
        registry, entry = _registry()
        service = self._service(storage, registry)
        body = service.enqueue("g-2", None, processor="tower")
        assert body["processor"] == "tower"
        run = service._find("g-2")
        assert run is not None
        assert run.processor == "tower"
        assert run.entry is entry
        service.cancel("g-2", "tower")

    def test_only_the_machine_being_measured_is_held(self, storage: Path) -> None:
        """Spec section 3 rule 5: the processor the bench is FOR."""
        registry, entry = _registry()
        worker = _FakeWorker()
        service = self._service(storage, registry, worker)
        service.enqueue("g-2", None, processor="tower")
        deadline = time.monotonic() + 10
        while not worker.held and time.monotonic() < deadline:
            time.sleep(0.02)
        assert worker.held == ["tower"]
        service.cancel("g-2", "tower")
        deadline = time.monotonic() + 10
        while not worker.released and time.monotonic() < deadline:
            time.sleep(0.02)
        assert worker.released == ["tower"]

    def test_the_same_row_may_queue_on_two_machines(self, storage: Path) -> None:
        registry, _entry = _registry()
        box = registry.register(username="box", name="box", host={},
                                catalog=TOWER_CATALOG, max_sessions=1)
        box.stream_open = True
        service = self._service(storage, registry)
        service.enqueue("g-2", None, processor="tower")
        second = service.enqueue("g-2", None, processor="box")
        assert second["processor"] == "box"
        with pytest.raises(BenchError) as excinfo:
            service.enqueue("g-2", None, processor="box")
        assert excinfo.value.status == 409
        service.cancel("g-2", "box")
        service.cancel("g-2", "tower")

    def test_reads_and_cancels_are_one_machines(self, storage: Path) -> None:
        """B6: the same row queued on tower (here: the worker's own
        autobench) must not be what a read or a Cancel for THIS server
        finds -- nor what a read for tower finds once THIS server's is
        done. None is this server's, as for a POST."""
        registry, _entry = _registry()
        service = self._service(storage, registry)
        service.enqueue("g-2", None, processor="tower", autobench=True)
        assert service.get("g-2")["state"] == "idle", "nothing of THIS server's"
        assert service.get("g-2", "local")["state"] == "idle"
        tower = service.get("g-2", "tower")
        assert tower["state"] in ("queued", "running")
        assert tower["processor"] == "tower" and tower["autobench"] is True
        with pytest.raises(BenchError):
            service.cancel("g-2")
        assert service.get("g-2", "tower")["state"] in ("queued", "running"), (
            "a Cancel for this server never cancels tower's autobench"
        )
        service.cancel("g-2", "tower")

    def test_one_machines_result_is_never_read_as_anothers(self, storage: Path) -> None:
        registry, entry = _registry()
        service = self._service(storage, registry)
        service._finish(_remote_run(service, entry, autobench=False), "done")
        assert service.get("g-2", "tower")["best"]["stage_workers"] == {"detect": 3}
        mine = service.get("g-2")
        assert mine["state"] == "idle" and mine.get("best") is None, (
            "tower's widths are not this server's result"
        )

    def test_the_http_api_reads_and_cancels_the_machine_it_names(
        self, storage: Path
    ) -> None:
        """The admin page names the machine on GET and DELETE
        (`?processor=`); without one it is this server's."""
        from mokuro_bunko.admin.api import AdminAPI

        registry, _entry = _registry()
        service = self._service(storage, registry)
        service.enqueue("g-2", None, processor="tower")
        api = AdminAPI.__new__(AdminAPI)
        api._bench_service = lambda: service  # type: ignore[method-assign]
        api._json_response = lambda start, status, body: [  # type: ignore[method-assign]
            json.dumps({"status": status, **body}).encode()
        ]

        def call(method: str, query: str) -> dict[str, Any]:
            environ = {"QUERY_STRING": query, "wsgi.input": io.BytesIO(b"")}
            raw = b"".join(api._handle_bench(environ, lambda *a: None, "g-2", method))
            return json.loads(raw)

        assert call("GET", "")["state"] == "idle"
        assert call("GET", "processor=tower")["state"] in ("queued", "running")
        assert call("DELETE", "")["status"] == 400
        assert call("DELETE", "processor=tower")["state"] == "cancelled"

    def test_the_sample_is_packed_outside_the_library_tree(self, storage: Path) -> None:
        registry, _entry = _registry()
        service = self._service(storage, registry)
        sample = build_sample(storage, 4)
        try:
            archive = service._pack_sample(sample, "bench-g-2")
            assert archive.parent == storage / ".processing"
            assert (storage / "library") not in archive.parents
            assert archive.name == bench_sample_filename("bench-g-2")
            with zipfile.ZipFile(archive) as zf:
                assert len(zf.namelist()) == sample.pages
        finally:
            sample.cleanup()


def _remote_run(service: BenchService, entry: Any, *, autobench: bool) -> Any:
    from mokuro_bunko.ocr.bench import _BenchRun, _spec_payload

    row = service._row("g-2")
    assert row is not None
    run = _BenchRun("g-2", row, 8, draft=False, spec=_spec_payload(row),
                    processor="tower", entry=entry, autobench=autobench)
    run.update(
        best={"pages_per_second": 41.2, "window_seconds": 24.1, "gpu_busy_pct": 88,
              "stage_workers": {"detect": 3}, "stage_device": {"engine": "gpu:0"}},
        host={"gpu": "RTX 4090"},
        # What the recognizer ran at, as a runner since the precision policy says.
        precision="fp32",
    )
    return run


class TestWhereTheResultLands:
    def test_a_finished_remote_bench_lands_in_that_processors_profile_only(
        self, storage: Path
    ) -> None:
        registry, entry = _registry()
        service = TestChoosingAProcessor._service(storage, registry)
        run = _remote_run(service, entry, autobench=False)
        service._finish(run, "done")
        row = ProcessorProfiles(storage).row("tower", "g-2")
        assert row is not None and row.bench is not None
        assert row.bench["pages_per_second"] == 41.2
        assert row.bench["host"] == {"gpu": "RTX 4090"}
        assert row.pools == {}, "a bench the user asked for applies nothing by itself"
        assert not bench_path(storage).exists(), (
            "a processor's number never lands in the row's own bench file"
        )

    def test_an_autobench_applies_the_best_widths_to_that_processor(
        self, storage: Path
    ) -> None:
        registry, entry = _registry()
        service = TestChoosingAProcessor._service(storage, registry)
        settled: list[tuple[str, Any]] = []
        run = _remote_run(service, entry, autobench=True)
        run.on_done = lambda state, ran_on: settled.append((state, ran_on))
        service._finish(run, "done")
        row = ProcessorProfiles(storage).row("tower", "g-2")
        assert row is not None
        assert row.pools["stage_workers"] == {"detect": 3}
        assert row.pools["stage_device"] == {"engine": "gpu:0"}
        assert settled == [("done", entry)], "told the registration it ran on"

    def test_an_autobench_never_replaces_pools_saved_while_it_ran(
        self, storage: Path
    ) -> None:
        """B2: `best` holds only what differs from the derivation; written
        over an admin's hand-set pools it would silently throw them away."""
        registry, entry = _registry()
        service = TestChoosingAProcessor._service(storage, registry)
        row = service._row("g-2")
        assert row is not None
        saved = {"stage_workers": {"detect": 6}, "queue_capacity": {},
                 "stage_device": {"detect": "cpu"}}
        run = _remote_run(service, entry, autobench=True)
        ProcessorProfiles(storage).set_pools("tower", "g-2", saved,
                                             recipe=row.output_affecting())
        service._finish(run, "done")
        stored = ProcessorProfiles(storage).row("tower", "g-2",
                                                recipe=row.output_affecting())
        assert stored is not None
        assert stored.pools == saved, "the admin's pools stay"
        assert stored.bench is not None and stored.bench["pages_per_second"] == 41.2


    def test_an_autobench_that_changed_nothing_leaves_the_row_s_own_table(
        self, storage: Path
    ) -> None:
        """A best that IS the measured spec is nothing to apply: writing it as
        this machine's pools would only freeze today's table there, and a
        later edit of the row would never reach it."""
        registry, entry = _registry()
        service = TestChoosingAProcessor._service(storage, registry)
        run = _remote_run(service, entry, autobench=True)
        run.update(best={"pages_per_second": 3.57, "window_seconds": 33.3,
                         "stage_workers": {}, "queue_capacity": {}, "stage_device": {},
                         "same_as_spec": True})
        service._finish(run, "done")
        row = ProcessorProfiles(storage).row("tower", "g-2")
        assert row is not None and row.bench is not None, "the number is kept"
        assert row.pools == {}, "and the row's own table still decides"


class TestAutoBench:
    @staticmethod
    def _worker(storage: Path, registry: ProcessorRegistry, *, autobench: bool) -> Any:
        from mokuro_bunko.ocr.watcher import OCRWorker

        worker = OCRWorker(
            storage_path=storage,
            poll_interval=30.0,
            generations=parse_generation_list([dict(r) for r in ROWS]),
            engines_python_path=Path(sys.executable),
            remote=registry,
            local_processing=False,
            autobench=autobench,
        )
        worker.bench_service = object()
        return worker

    def test_a_pair_with_no_profile_entry_needs_one(self, storage: Path) -> None:
        registry, entry = _registry()
        worker = self._worker(storage, registry, autobench=True)
        row = worker.generations[1]
        assert worker.autobench_needed(entry, row) is True
        ProcessorProfiles(storage).set_bench("tower", row.id, {"precision": "fp32", "pages_per_second": 2.0},
                                             recipe=row.output_affecting())
        assert worker.autobench_needed(entry, row) is False

    def test_an_entry_holding_only_saved_pools_is_not_benchmarked_over(
        self, storage: Path
    ) -> None:
        """B2, spec section 4: only a pair with NO profile entry is
        benchmarked first. Pools an admin saved for that machine ARE an
        entry, bench or no bench."""
        registry, entry = _registry()
        worker = self._worker(storage, registry, autobench=True)
        row = worker.generations[1]
        ProcessorProfiles(storage).set_pools(
            "tower", row.id,
            {"stage_workers": {"detect": 6}, "queue_capacity": {},
             "stage_device": {"detect": "cpu"}},
            recipe=row.output_affecting(),
        )
        assert worker.autobench_needed(entry, row) is False

    def test_a_row_whose_recipe_changed_is_measured_again(self, storage: Path) -> None:
        registry, entry = _registry()
        worker = self._worker(storage, registry, autobench=True)
        row = worker.generations[1]
        ProcessorProfiles(storage).set_bench("tower", row.id, {"precision": "fp32", "pages_per_second": 2.0},
                                             recipe=("hayai-nova", "ppocr-manga", 512))
        assert worker.autobench_needed(entry, row) is True

    def test_with_autobench_off_nothing_is_ever_needed(self, storage: Path) -> None:
        registry, entry = _registry()
        worker = self._worker(storage, registry, autobench=False)
        assert worker.autobench_needed(entry, worker.generations[1]) is False

    def test_without_a_bench_service_nothing_is_ever_needed(self, storage: Path) -> None:
        registry, entry = _registry()
        worker = self._worker(storage, registry, autobench=True)
        worker.bench_service = None
        assert worker.autobench_needed(entry, worker.generations[1]) is False

    def test_a_pair_that_failed_its_benchmark_runs_untuned(self, storage: Path) -> None:
        """Otherwise a row whose benchmark cannot run is never claimable."""
        registry, entry = _registry()
        worker = self._worker(storage, registry, autobench=True)
        row = worker.generations[1]
        worker._autobench_settled(entry.name, row.id, "failed")
        assert worker.autobench_needed(entry, row) is False

    def test_a_processor_that_left_mid_bench_is_asked_again(self, storage: Path) -> None:
        registry, entry = _registry()
        worker = self._worker(storage, registry, autobench=True)
        row = worker.generations[1]
        registry.drop(entry.processor_id, "stream closed")
        worker._autobench_settled(entry.name, row.id, "failed")
        again = registry.register(username="tower", name="tower", host={},
                                  catalog=TOWER_CATALOG, max_sessions=1)
        again.stream_open = True
        assert worker.autobench_needed(again, row) is True

    def test_a_processor_back_before_its_benchmark_settles_is_asked_again(
        self, storage: Path
    ) -> None:
        """B5: a processor that drops re-registers under the same name within
        seconds -- usually before the library notices the old benchmark's body
        is gone. Whether THE REGISTRATION THE BENCH RAN ON left decides, not
        whether a processor of that name is connected when it settles; and
        the returning machine's slot asks for it again in the same scan."""
        from mokuro_bunko.ocr.bench import _BenchRun, _spec_payload

        registry, first = _registry()
        worker = self._worker(storage, registry, autobench=True)
        (storage / "library" / "Alpha" / "Volume 1.mokuro").write_text(
            json.dumps({"version": "0.0", "pages": [], "chars": 0}), encoding="utf-8"
        )
        row = worker.generations[1]
        asked: list[dict[str, Any]] = []

        class _Bench:
            @staticmethod
            def enqueue(key: str, spec: Any, processor: str = "local",
                        **kwargs: Any) -> dict[str, Any]:
                asked.append(kwargs)
                return {}

        worker.bench_service = _Bench()
        slot = worker._all_slots()[0]
        assert worker.claim_next(slot) is None
        worker._drain_autobench_requests()
        assert len(asked) == 1
        # The network blips: tower's stream closes and it is back at once,
        # BEFORE the old benchmark's body is found to be gone.
        registry.drop(first.processor_id, "stream closed")
        again = registry.register(username="tower", name="tower", host={},
                                  catalog=TOWER_CATALOG, max_sessions=1)
        again.stream_open = True
        # ...and then the benchmark that ran on the OLD registration settles.
        service = TestChoosingAProcessor._service(storage, registry)
        run = _BenchRun(row.id, row, 8, draft=False, spec=_spec_payload(row),
                        processor="tower", entry=first, autobench=True,
                        on_done=asked[0]["on_done"])
        service._finish(run, "failed")
        assert ("tower", row.id) not in worker._autobench_failed, (
            "a machine that left is not a machine that cannot be measured"
        )
        assert worker.autobench_needed(again, row) is True
        slot_again = next(s for s in worker._all_slots() if s.processor_id == again.processor_id)
        assert worker.claim_next(slot_again) is None, "still measured first"
        worker._drain_autobench_requests()
        assert len(asked) == 2, "the returning machine asked again in the same scan"

    def test_a_request_is_recorded_under_the_lock_and_fired_outside_it(
        self, storage: Path
    ) -> None:
        """`BenchService.enqueue` takes the worker lock; `_claim` holds it."""
        registry, entry = _registry()
        worker = self._worker(storage, registry, autobench=True)
        row = worker.generations[1]
        asked: list[tuple[str, str]] = []

        class _Bench:
            @staticmethod
            def enqueue(key: str, spec: Any, processor: str = "local",
                        **kwargs: Any) -> dict[str, Any]:
                assert kwargs["autobench"] is True
                asked.append((key, processor))
                return {}

        worker.bench_service = _Bench()
        worker._want_autobench(entry, row)
        assert asked == [], "nothing is enqueued while the lock may be held"
        worker._drain_autobench_requests()
        assert asked == [(row.id, "tower")]
        worker._want_autobench(entry, row)
        worker._drain_autobench_requests()
        assert asked == [(row.id, "tower")], "asked once while it is in flight"

    def test_a_claim_waits_for_the_benchmark_and_then_runs_measured(
        self, storage: Path
    ) -> None:
        """Spec section 4: benchmarked first, `best` applied, only then offered."""
        registry, entry = _registry()
        worker = self._worker(storage, registry, autobench=True)
        # The primary layer is there: only the hayai row is pending.
        (storage / "library" / "Alpha" / "Volume 1.mokuro").write_text(
            json.dumps({"version": "0.0", "pages": [], "chars": 0}), encoding="utf-8"
        )
        row = worker.generations[1]
        settle: list[Callable[[str], None]] = []

        class _Bench:
            @staticmethod
            def enqueue(key: str, spec: Any, processor: str = "local",
                        **kwargs: Any) -> dict[str, Any]:
                settle.append(kwargs["on_done"])
                return {}

        worker.bench_service = _Bench()
        slot = worker._all_slots()[0]
        assert worker.claim_next(slot) is None, "not before it is measured"
        worker._drain_autobench_requests()
        assert len(settle) == 1
        assert worker.claim_next(slot) is None, "not while the benchmark runs"
        ProcessorProfiles(storage).set_bench("tower", row.id, {"precision": "fp32", "pages_per_second": 5.0},
                                             recipe=row.output_affecting())
        settle[0]("done")
        job = worker.claim_next(slot)
        assert job is not None and job[1] == row.id


@pytest.fixture
def cli_row(monkeypatch: pytest.MonkeyPatch) -> Any:
    """A row that reads a volume behind its own command line (no road).

    No shipped engine is one today -- both mokuro engines are served -- so
    the engine is registered for the test alone.
    """
    from mokuro_bunko.ocr import engines
    from mokuro_bunko.ocr.generations import GenerationSpec

    monkeypatch.setitem(
        engines.ENGINES, "mokuro-cli",
        engines.EngineSpec(id="mokuro-cli", label="mokuro CLI",
                           recognizer="kha-white/manga-ocr-base", uses_mokuro_env=True),
    )
    row = GenerationSpec(id="g-3", name="cli", engine="mokuro-cli")
    assert row.monolithic
    return row


class TestAMonolithicRowOnAProcessor:
    """N3: a monolithic row has no pipeline for ``--bench`` to tune. Sending
    it to a processor failed every time -- and, the failure being remembered
    only in memory, once more after every restart."""

    def test_it_never_needs_an_autobench(self, storage: Path, cli_row: Any) -> None:
        registry, entry = _registry()
        worker = TestAutoBench._worker(storage, registry, autobench=True)
        assert worker.autobench_needed(entry, cli_row) is False
        assert worker.autobench_needed(entry, worker.generations[1]) is True

    def test_a_benchmark_of_it_on_a_processor_is_refused_up_front(
        self, storage: Path, cli_row: Any
    ) -> None:
        registry = ProcessorRegistry(local_name="this server")
        entry = registry.register(
            username="tower", name="tower", host={},
            catalog={**TOWER_CATALOG, "engines": [*TOWER_CATALOG["engines"], "mokuro-cli"]},
            max_sessions=1,
        )
        entry.stream_open = True
        rows = [*parse_generation_list([dict(row) for row in ROWS]), cli_row]
        worker = _FakeWorker()
        service = BenchService(
            storage, worker=lambda: worker, generations=lambda: rows,
            processors=registry.entries, profiles=ProcessorProfiles(storage),
        )
        settled: list[str] = []
        with pytest.raises(BenchError) as excinfo:
            service.enqueue("g-3", None, processor="tower", autobench=True,
                            on_done=settled.append)
        assert excinfo.value.status == 400
        assert "command line" in excinfo.value.message
        assert worker.held == [], "nothing was held for it"
        assert service._find("g-3") is None
        assert ProcessorProfiles(storage).row("tower", "g-3") is None, "nothing persisted"
        assert not (storage / ".processing").exists(), "no sample was packed"


class TestTheSampleRoute:
    @staticmethod
    def _get(api: ProcessorAPI, path: str, *, method: str = "GET",
             range_header: str = "", username: str = "tower") -> tuple[int, dict[str, str],
                                                                       bytes]:
        environ = {
            "REQUEST_METHOD": method, "PATH_INFO": path, "wsgi.input": io.BytesIO(b""),
            "mokuro.role": "processor", "mokuro.username": username,
        }
        if range_header:
            environ["HTTP_RANGE"] = range_header
        seen: dict[str, Any] = {}

        def start_response(status: str, headers: list[tuple[str, str]]) -> None:
            seen["status"] = int(status.split()[0])
            seen["headers"] = dict(headers)

        body = b"".join(api(environ, start_response))
        return seen["status"], seen["headers"], body

    @staticmethod
    def _api(tmp_path: Path) -> tuple[ProcessorAPI, Any, bytes]:
        registry, entry = _registry()
        samples = tmp_path / ".processing"
        samples.mkdir()
        blob = bytes(range(256)) * 4
        (samples / bench_sample_filename("bench-g-2")).write_bytes(blob)
        api = ProcessorAPI(lambda e, s: [], registry, samples_dir=samples)
        return api, entry, blob

    def test_the_processor_the_bench_is_for_can_read_it_ranged(
        self, tmp_path: Path
    ) -> None:
        api, entry, blob = self._api(tmp_path)
        bench = RemoteBench(entry, bid="bench-g-2", spec={}, sample_url="/x", pages=4)
        bench.start()
        base = f"/_processor/{entry.processor_id}/bench/bench-g-2/sample"
        status, headers, body = self._get(api, base)
        assert (status, body) == (200, blob)
        status, headers, body = self._get(api, base, method="HEAD")
        assert status == 200 and body == b"" and headers["Content-Length"] == str(len(blob))
        status, headers, body = self._get(api, base, range_header="bytes=10-19")
        assert status == 206 and body == blob[10:20]
        assert headers["Content-Range"] == f"bytes 10-19/{len(blob)}"
        status, _headers, body = self._get(api, base, range_header="bytes=-16")
        assert status == 206 and body == blob[-16:]

    def test_no_bench_running_no_sample(self, tmp_path: Path) -> None:
        api, entry, _blob = self._api(tmp_path)
        status, _headers, _body = self._get(
            api, f"/_processor/{entry.processor_id}/bench/bench-g-2/sample"
        )
        assert status == 404

    def test_another_account_cannot_read_it(self, tmp_path: Path) -> None:
        api, entry, _blob = self._api(tmp_path)
        RemoteBench(entry, bid="bench-g-2", spec={}, sample_url="/x", pages=4).start()
        status, _headers, _body = self._get(
            api, f"/_processor/{entry.processor_id}/bench/bench-g-2/sample",
            username="mallory",
        )
        assert status == 404

    def test_a_bid_cannot_name_a_path(self) -> None:
        assert bench_sample_filename("bench-../../etc/passwd") == "bench-etcpasswd.cbz"


class TestTheTrialsNumbers:
    def test_a_remote_trial_keeps_the_busy_percent_its_machine_measured(
        self, storage: Path
    ) -> None:
        registry, entry = _registry()
        service = TestChoosingAProcessor._service(storage, registry)
        run = _remote_run(service, entry, autobench=False)

        class _Session:
            def __init__(self) -> None:
                self.events = [
                    {"event": "bench_ready", "pages": 8},
                    {"event": "bench_trial", "n": 1, "pages_per_second": 4.0,
                     "first_emission_at": 1.0, "last_emission_at": 9.0,
                     "gpu_busy_pct": 88.0, "cpu_busy_pct": 20.0},
                    {"event": "bench_done", "precision": "fp32", "best": {"trial": 1, "pages_per_second": 4.0}},
                    {"event": "exit", "returncode": 0},
                ]

            def start(self) -> bool:
                return True

            def poll_event(self, timeout: float | None = None) -> dict[str, Any] | None:
                return self.events.pop(0) if self.events else None

            def kill(self) -> bool:
                return True

            def wait(self, timeout: float | None = None) -> int:
                return 0

            def stderr_tail(self) -> None:
                return None

        class _Sampler:
            def means(self, first: float, last: float) -> dict[str, float | None]:
                return {"gpu_busy_pct": 3.0, "cpu_busy_pct": 1.0}

        sample = build_sample(storage, 4)
        try:
            service._read_composed(run, _Session(), _Sampler(), 0.0, sample)
        finally:
            sample.cleanup()
        trial = run.data["trials"][0]
        assert trial["gpu_busy_pct"] == 88.0
        assert trial["cpu_busy_pct"] == 20.0



class TestTheMachineAsItIsNow:
    def test_a_processor_that_reconnected_is_measured_on_its_new_registration(
        self, storage: Path
    ) -> None:
        registry, old = _registry()
        service = TestChoosingAProcessor._service(storage, registry)
        run = _remote_run(service, old, autobench=False)
        new = registry.register(username="tower", name="tower", host={"gpu": "RTX 4090"},
                                catalog=TOWER_CATALOG, max_sessions=1)
        new.stream_open = True
        assert old.dropped, "the reconnect replaced the old registration"
        sample = build_sample(storage, 4)
        seen: list[Any] = []

        def read(run_: Any, bench: Any, *args: Any) -> None:
            seen.append(bench.entry)

        service._read_composed = read  # type: ignore[method-assign]
        try:
            service._run_remote(run, sample)
        finally:
            sample.cleanup()
        assert seen == [new]

    def test_a_processor_that_is_gone_fails_the_benchmark(self, storage: Path) -> None:
        registry, entry = _registry()
        service = TestChoosingAProcessor._service(storage, registry)
        run = _remote_run(service, entry, autobench=False)
        registry.drop(entry.processor_id, "stream closed")
        sample = build_sample(storage, 4)
        try:
            with pytest.raises(BenchError, match="no longer connected"):
                service._run_remote(run, sample)
        finally:
            sample.cleanup()


class TestABenchmarksBodyOnTheWire:
    def test_a_body_that_ends_after_bench_done_leaves_the_processor_connected(
        self,
    ) -> None:
        """The events sink drops a processor whose LIVE session's body ends
        without an `exit`. A benchmark is over at `bench_done`, so its body
        ending then is not the processor leaving."""
        from mokuro_bunko.ocr.remote.protocol import encode_frame

        registry, entry = _registry()
        api = ProcessorAPI(lambda e, s: [], registry)
        bench = RemoteBench(entry, bid="bench-g-2", spec={}, sample_url="/x", pages=4)
        bench.start()
        frames = encode_frame({"event": "bench_ready", "pages": 4}) + encode_frame(
            {"event": "bench_done", "precision": "fp32", "best": {"trial": 1}}
        )
        environ = {
            "REQUEST_METHOD": "POST",
            "PATH_INFO": f"/_processor/{entry.processor_id}/sessions/bench-g-2/events",
            "wsgi.input": io.BytesIO(frames),
            "mokuro.role": "processor", "mokuro.username": "tower",
        }
        statuses: list[str] = []
        b"".join(api(environ, lambda status, headers: statuses.append(status)))
        assert statuses[0].startswith("200")
        assert not entry.dropped
        kinds = [bench.poll_event(timeout=1.0)["event"] for _ in range(3)]
        assert kinds == ["bench_ready", "bench_done", "exit"]

    def test_a_body_that_ends_mid_benchmark_is_the_processor_leaving(self) -> None:
        from mokuro_bunko.ocr.remote.protocol import encode_frame

        registry, entry = _registry()
        api = ProcessorAPI(lambda e, s: [], registry)
        RemoteBench(entry, bid="bench-g-2", spec={}, sample_url="/x", pages=4).start()
        environ = {
            "REQUEST_METHOD": "POST",
            "PATH_INFO": f"/_processor/{entry.processor_id}/sessions/bench-g-2/events",
            "wsgi.input": io.BytesIO(encode_frame({"event": "bench_ready", "pages": 4})),
            "mokuro.role": "processor", "mokuro.username": "tower",
        }
        b"".join(api(environ, lambda status, headers: None))
        assert entry.dropped


# -- what an auto-benchmark stores, read the way the runner reports it --------


class _ScriptedBench:
    """A remote benchmark's event stream, already written."""

    def __init__(self, events: list[dict[str, Any]]) -> None:
        self.events = list(events)

    def start(self) -> bool:
        return True

    def poll_event(self, timeout: float = 0.0) -> dict[str, Any] | None:
        del timeout
        return self.events.pop(0) if self.events else None

    def kill(self) -> None:
        pass

    def wait(self, timeout: float = 0.0) -> None:
        del timeout

    def stderr_tail(self) -> str:
        return ""


QUEUED_ROWS = [
    ROWS[0],
    {**ROWS[1], "pools": {"queue_capacity": {"engine": 4, "post": 4},
                          "stage_device": {"detect": "cpu"}}},
]


class TestWhatAnAutobenchStores:
    """The runner's ``best`` is not a table: its ``queue_capacity`` is always
    empty (the search never chooses one) and its ``stage_device`` names only
    what moved. Stored as the machine's pools, the row's queue pins were
    dropped there and frozen out of the reach of any later edit of the row.

    Nor is the runner ever GIVEN the row's capacities (``open_bench``), so a
    queue pin is kept only where the winning trial's derived capacity landed
    on it; anywhere else it is a capacity nothing measured, stored ``auto``."""

    @staticmethod
    def _run(
        storage: Path, best: dict[str, Any], ran: dict[str, int] | None = None
    ) -> Any:
        """``ran``: the capacities the winning trial reports it ran at --
        by default the row's own pins, so the derivation landed on them."""
        from mokuro_bunko.ocr.bench import _BenchRun, _spec_payload

        if ran is None:
            ran = {"detect": 2, "engine": 4, "post": 4}
        registry, entry = _registry()
        rows = parse_generation_list([dict(row) for row in QUEUED_ROWS])
        service = BenchService(
            storage, worker=lambda: _FakeWorker(), generations=lambda: rows,
            processors=registry.entries, profiles=ProcessorProfiles(storage),
        )
        row = rows[1]
        run = _BenchRun(row.id, row, 8, draft=False, spec=_spec_payload(row),
                        processor="tower", entry=entry, autobench=True)
        session = _ScriptedBench([
            {"event": "bench_ready", "pages": 8, "tunable": True, "startup_seconds": 6.6,
             "stage_device": {"detect": "cpu", "engine": "gpu:0"}},
            {"event": "bench_trial", "n": 1, "pages_per_second": 3.57,
             "window_seconds": 33.3, "gpu_busy_pct": 61.3, "cpu_busy_pct": 35.3,
             "queue_capacity": dict(ran)},
            {"event": "bench_done", "precision": "fp32",
             "best": {"trial": 1, "pages_per_second": 3.57, "window_seconds": 33.3,
                      "queue_capacity": {}, "stage_device": {}, **best}},
            {"event": "exit", "returncode": 0},
        ])
        sample = build_sample(storage, 4)
        try:
            sampler = service._sampler(None).start()
            try:
                service._read_composed(run, session, sampler, time.monotonic(), sample)
            finally:
                sampler.stop()
        finally:
            sample.cleanup()
        assert run.data["state"] == "done", run.data
        stored = ProcessorProfiles(storage).row("tower", row.id,
                                                recipe=row.output_affecting())
        assert stored is not None and stored.bench is not None
        return stored

    def test_one_that_changed_nothing_leaves_the_row_s_own_table(
        self, storage: Path
    ) -> None:
        stored = self._run(storage, {"stage_workers": {}})
        assert stored.pools == {}, "the row's own table, queue pins and all, still decides"

    def test_one_that_widened_a_stage_keeps_the_row_s_queue_pins(
        self, storage: Path
    ) -> None:
        stored = self._run(storage, {"stage_workers": {"detect": 3}})
        assert stored.pools == {
            "stage_workers": {"detect": 3},
            "queue_capacity": {"engine": 4, "post": 4},
            "stage_device": {"detect": "cpu"},
        }

    def test_a_queue_pin_the_winner_did_not_run_is_stored_as_derived(
        self, storage: Path
    ) -> None:
        """The runner is never given the row's capacities (``open_bench``),
        so the winner ran DERIVED ones -- here engine 1, post 2. Copying the
        row's ``{engine: 4, post: 4}`` into what is stored pins capacities
        the benchmark never measured."""
        stored = self._run(storage, {"stage_workers": {"detect": 3}},
                           {"detect": 3, "engine": 1, "post": 2})
        assert stored.pools["queue_capacity"] == {"engine": "auto", "post": "auto"}

    def test_each_queue_pin_is_read_against_what_ran(self, storage: Path) -> None:
        stored = self._run(storage, {"stage_workers": {"detect": 3}},
                           {"detect": 3, "engine": 4, "post": 2})
        assert stored.pools["queue_capacity"] == {"engine": 4, "post": "auto"}

    def test_queue_pins_that_did_not_run_are_a_change_to_the_spec(
        self, storage: Path
    ) -> None:
        """Widths and placement as the row has them, capacities derived and
        different: the benchmark measured something the row does not say,
        so it is not the spec."""
        stored = self._run(storage, {"stage_workers": {}},
                           {"detect": 2, "engine": 1, "post": 1})
        assert stored.pools == {
            "stage_workers": {},
            "queue_capacity": {"engine": "auto", "post": "auto"},
            "stage_device": {"detect": "cpu"},
        }

    def test_a_winner_that_did_not_say_its_capacities_keeps_none_of_the_pins(
        self, storage: Path
    ) -> None:
        stored = self._run(storage, {"stage_workers": {"detect": 3}}, {})
        assert stored.pools["queue_capacity"] == {"engine": "auto", "post": "auto"}


WIDTH_ROWS = [ROWS[0], {**ROWS[1], "pools": {"stage_workers": {"post": 1}}}]


class TestAnAutobenchThatKeptTheDerivedWidths:
    """The runner is never given the row's widths (``open_bench``), so when
    the derived widths win (``best.stage_workers == {}``) the row's pinned
    ``post: 1`` is a width the benchmark never measured. Stored as three
    empty tables that is no opinion (`profiles.holds_pools`), and the machine
    ran the row's ``post: 1`` after all -- a setting nothing measured."""

    @staticmethod
    def _run(storage: Path, best: dict[str, Any], ran: dict[str, int]) -> Any:
        from mokuro_bunko.ocr.bench import _BenchRun, _spec_payload

        registry, entry = _registry()
        rows = parse_generation_list([dict(row) for row in WIDTH_ROWS])
        service = BenchService(
            storage, worker=lambda: _FakeWorker(), generations=lambda: rows,
            processors=registry.entries, profiles=ProcessorProfiles(storage),
        )
        row = rows[1]
        run = _BenchRun(row.id, row, 8, draft=False, spec=_spec_payload(row),
                        processor="tower", entry=entry, autobench=True)
        session = _ScriptedBench([
            {"event": "bench_ready", "pages": 8, "tunable": True, "startup_seconds": 6.6,
             "stage_device": {"detect": "cpu", "engine": "gpu:0"}},
            {"event": "bench_trial", "n": 1, "pages_per_second": 3.57,
             "window_seconds": 33.3, "stage_workers": dict(ran),
             "queue_capacity": {}, "stage_device": {"detect": "cpu", "engine": "gpu:0"}},
            {"event": "bench_done", "precision": "fp32",
             "best": {"trial": 1, "pages_per_second": 3.57, "window_seconds": 33.3,
                      "queue_capacity": {}, "stage_device": {}, **best}},
            {"event": "exit", "returncode": 0},
        ])
        sample = build_sample(storage, 4)
        try:
            sampler = service._sampler(None).start()
            try:
                service._read_composed(run, session, sampler, time.monotonic(), sample)
            finally:
                sampler.stop()
        finally:
            sample.cleanup()
        assert run.data["state"] == "done", run.data
        stored = ProcessorProfiles(storage).row("tower", row.id,
                                                recipe=row.output_affecting())
        assert stored is not None and stored.bench is not None
        return run.data["best"], stored

    def test_derived_widths_that_won_are_stored_as_derived(self, storage: Path) -> None:
        best, stored = self._run(storage, {"stage_workers": {}},
                                 {"detect": 1, "engine": 1, "post": 3})
        assert best["stage_workers"] == {"post": "auto"}
        assert best["same_as_spec"] is False
        assert stored.pools == {
            "stage_workers": {"post": "auto"},
            "queue_capacity": {},
            "stage_device": {},
        }

    def test_a_derived_width_that_is_the_row_s_pin_is_the_spec(self, storage: Path) -> None:
        best, stored = self._run(storage, {"stage_workers": {}},
                                 {"detect": 1, "engine": 1, "post": 1})
        assert best["stage_workers"] == {"post": 1}
        assert best["same_as_spec"] is True
        assert stored.pools == {}, "the row's own table was what was measured"

    def test_a_widened_stage_keeps_the_rest_derived(self, storage: Path) -> None:
        best, stored = self._run(storage, {"stage_workers": {"detect": 3}},
                                 {"detect": 3, "engine": 1, "post": 2})
        assert stored.pools["stage_workers"] == {"detect": 3, "post": "auto"}


class TestOneLinePerMachine:
    """Benchmarks on DIFFERENT machines run at the same time, and each
    machine goes back to work as soon as its own benchmarks are done.

    Measured live (a paddle-manga row enabled with three processors
    connected): the three autobenchmarks ran one after another -- rig-c
    00:30:43-00:35:39, tower to 00:36:38, server to 00:40:07 -- and every
    machine stayed held until the LAST of them ended, so tower sat idle for
    3.5 minutes and rig-c for 4.5 behind benchmarks of other hardware.
    """

    @staticmethod
    def _gated(service: BenchService) -> tuple[dict[str, Any], list[str]]:
        """Replace the measuring with a gate per machine; record who started."""
        import threading

        gates: dict[str, threading.Event] = {}
        started: list[str] = []
        lock = threading.Lock()

        def fake_run_one(run: Any, worker: Any) -> None:
            del worker
            with lock:
                gate = gates.setdefault(run.key + "@" + run.processor, threading.Event())
                started.append(run.key + "@" + run.processor)
            run.update(state="running")
            gate.wait(timeout=20)
            service._finish(run, "done")

        service._run_one = fake_run_one  # type: ignore[method-assign]
        return gates, started

    @staticmethod
    def _until(condition: Callable[[], bool], what: str) -> None:
        deadline = time.monotonic() + 10
        while not condition():
            if time.monotonic() > deadline:
                raise AssertionError(f"timed out waiting for {what}")
            time.sleep(0.02)

    @staticmethod
    def _open(gates: dict[str, Any], name: str) -> None:
        import threading

        gates.setdefault(name, threading.Event()).set()

    def _two_machines(self, storage: Path) -> tuple[BenchService, _FakeWorker]:
        registry, _entry = _registry()
        box = registry.register(username="box", name="box", host={},
                                catalog=TOWER_CATALOG, max_sessions=1)
        box.stream_open = True
        worker = _FakeWorker()
        return TestChoosingAProcessor._service(storage, registry, worker), worker

    def test_two_machines_are_measured_at_the_same_time(self, storage: Path) -> None:
        service, _worker = self._two_machines(storage)
        gates, started = self._gated(service)
        service.enqueue("g-2", None, processor="tower", autobench=True)
        service.enqueue("g-2", None, processor="box", autobench=True)
        self._until(lambda: sorted(started) == ["g-2@box", "g-2@tower"],
                    "both machines' benchmarks to start")
        assert service.get("g-2", "tower")["position"] == 0
        assert service.get("g-2", "box")["position"] == 0, "running, not second in line"
        self._open(gates, "g-2@tower")
        self._open(gates, "g-2@box")

    def test_a_machine_is_released_when_its_own_benchmarks_are_done(
        self, storage: Path
    ) -> None:
        service, worker = self._two_machines(storage)
        gates, started = self._gated(service)
        service.enqueue("g-2", None, processor="tower", autobench=True)
        service.enqueue("g-2", None, processor="box", autobench=True)
        self._until(lambda: len(started) == 2, "both benchmarks to start")
        self._open(gates, "g-2@tower")
        self._until(lambda: worker.released == ["tower"], "tower to be released")
        assert service.get("g-2", "box")["state"] == "running", (
            "box is still being measured; tower went back to work anyway"
        )
        self._open(gates, "g-2@box")
        self._until(lambda: sorted(worker.released) == ["box", "tower"], "box's release")
        assert sorted(worker.held) == ["box", "tower"]

    def test_back_to_back_benchmarks_on_one_machine_hold_it_once(
        self, storage: Path
    ) -> None:
        """The reason a line holds its machine across benchmarks: volumes it
        interrupted must not restart between two measurements of one card."""
        service, worker = self._two_machines(storage)
        gates, started = self._gated(service)
        service.enqueue("g-1", None, processor="tower", autobench=True)
        service.enqueue("g-2", None, processor="tower", autobench=True)
        self._until(lambda: started == ["g-1@tower"], "the first benchmark")
        assert service.get("g-2", "tower")["position"] == 1
        self._open(gates, "g-1@tower")
        self._until(lambda: started == ["g-1@tower", "g-2@tower"], "the second benchmark")
        assert worker.released == [], "still held between its two benchmarks"
        self._open(gates, "g-2@tower")
        self._until(lambda: worker.released == ["tower"], "one release at the end")
        assert worker.held == ["tower"]

    def test_a_benchmark_queued_as_a_line_ends_is_not_lost(self, storage: Path) -> None:
        service, worker = self._two_machines(storage)
        gates, started = self._gated(service)
        for _ in range(20):
            self._open(gates, "g-2@tower")
            self._open(gates, "g-1@tower")
            before = len(started)
            service.enqueue("g-2", None, processor="tower", autobench=True)
            self._until(lambda before=before: len(started) == before + 1, "the benchmark to run")
            self._until(lambda: service.get("g-2", "tower")["state"] == "done", "done")
        assert len(worker.held) == len(worker.released)


class TestWhatEachMachineIsConfiguring:
    def test_each_machine_with_a_line_says_what_it_measures(self, storage: Path) -> None:
        service, _worker = TestOneLinePerMachine()._two_machines(storage)
        gates, started = TestOneLinePerMachine._gated(service)
        service.enqueue("g-2", None, processor="tower", autobench=True)
        service.enqueue("g-1", None, processor="box")
        TestOneLinePerMachine._until(lambda: len(started) == 2, "both to start")
        assert service.configuring() == {
            "tower": {"key": "g-2", "generation": "hayai-ctd", "auto": True},
            "box": {"key": "g-1", "generation": "mokuro", "auto": False},
        }
        TestOneLinePerMachine._open(gates, "g-2@tower")
        TestOneLinePerMachine._open(gates, "g-1@box")
        TestOneLinePerMachine._until(lambda: service.configuring() == {}, "both to finish")
