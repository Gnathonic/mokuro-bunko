"""Per-row Benchmark & tune: the state machine, the HTTP face, the numbers.

Every number a person is shown about a row's speed has to have been measured
on THEIR machine on THEIR pages, so the things these tests pin down are:

* the sample is real pages, spread across series and volumes, cleaned up;
* a benchmark measures a SPEC -- the row as edited, saved or not (ADDENDUM
  5): `spec` present validates and measures it (ignoring name/primary/
  enabled/id), `spec` absent measures the saved row for `<key>`, `<key>` is
  a saved id or a client `draft-*` key whose result never touches disk;
* benchmarks QUEUE (ADDENDUM 6): 202 + `position` (0 = running), strict
  FIFO, 409 only for re-posting the SAME key while it is already
  queued/running, DELETE of a queued one shifts the rest up, DELETE of the
  running one starts the next;
* the OCR queue is PRE-EMPTED (not waited behind) once for the whole line
  and released only once the line drains -- `preempted` on the first
  benchmark of a line;
* every state transition (idle -> queued -> running -> done/failed/
  cancelled) and the exact bench object at each;
* the composed path composes the runner's ADDENDUM 4 events, and the
  monolithic path times one mokuro run and is not tunable;
* the derived fields the SERVER adds: host, sample, estimates,
  `best.same_as_spec`, `generation`/`key`, the timestamps, `waiting_for_queue`;
* persistence in `.ocr-bench.json` for a saved id only -- never a draft key.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import zipfile
from pathlib import Path
from typing import Any
from unittest.mock import patch

import pytest

from mokuro_bunko.ocr.bench import (
    BenchError,
    BenchService,
    build_sample,
    describe_host,
)
from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.processor import MokuroRunResult
from mokuro_bunko.ocr.watcher import OCRWorker

FAKE_RUNNER = Path(__file__).resolve().parents[1] / "fixtures" / "fake_runner.py"

PRIMARY: dict[str, Any] = {"name": "mokuro", "engine": "mokuro", "primary": True}
HAYAI: dict[str, Any] = {"name": "hayai-nova", "engine": "hayai-nova"}
HAYAI_SPEC: dict[str, Any] = {"engine": "hayai-nova"}


def _gens(*rows: dict[str, Any]) -> list[GenerationSpec]:
    return parse_generation_list([dict(row) for row in rows])


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    (tmp_path / "library").mkdir()
    (tmp_path / "inbox").mkdir()
    return tmp_path


def _library(storage: Path, **series: dict[str, int]) -> None:
    """``Alpha={"Volume 1": 5}``: a series, its volumes and their page counts."""
    for name, volumes in series.items():
        for volume, pages in volumes.items():
            path = storage / "library" / name / f"{volume}.cbz"
            path.parent.mkdir(parents=True, exist_ok=True)
            with zipfile.ZipFile(path, "w") as zf:
                for n in range(pages):
                    zf.writestr(f"page_{n:03d}.jpg", b"x" * 16)
                # The embedded thumbnail an uploader names after the archive
                # is not a page and must never be sampled as one.
                zf.writestr(f"{volume}.webp", b"thumb")


def _script(storage: Path, **rules: Any) -> Path:
    path = storage / "bench-script.json"
    path.write_text(json.dumps(rules), encoding="utf-8")
    return path


def _worker(storage: Path, rows: list[GenerationSpec], script: Path | None = None) -> OCRWorker:
    worker = OCRWorker(
        storage_path=storage,
        poll_interval=30.0,
        generations=rows,
        engines_python_path=Path(sys.executable),
        sessions=True,
    )
    processor = worker.processor
    real_bench = processor.open_bench
    real_env = processor.ocr_env

    def open_bench(generation: GenerationSpec, sample_dir: Path, log: Path, **kw: Any) -> Any:
        session = real_bench(generation, sample_dir, log, **kw)
        session.command[1] = str(FAKE_RUNNER)
        return session

    def ocr_env(generation: GenerationSpec | None = None) -> dict[str, str]:
        env = real_env(generation)
        if script is not None:
            env["FAKE_RUNNER_SCRIPT"] = str(script)
        return env

    processor.open_bench = open_bench  # type: ignore[method-assign]
    processor.ocr_env = ocr_env  # type: ignore[method-assign]
    return worker


def _service(
    storage: Path,
    rows: list[GenerationSpec],
    worker: OCRWorker | None,
    *,
    environment_problem: Any = lambda row: None,
    remaining_pages: Any = lambda row: None,
) -> BenchService:
    return BenchService(
        storage,
        worker=lambda: worker,
        generations=lambda: rows,
        backend=lambda: "rocm",
        environment_problem=environment_problem,
        remaining_pages=remaining_pages,
    )


def _wait_for(service: BenchService, key: str, state: str, timeout: float = 30.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        data = service.get(key)
        if data.get("state") == state:
            return data
        time.sleep(0.02)
    raise AssertionError(f"benchmark never reached {state!r}; last was {service.get(key)}")


def _wait_until_released(worker: OCRWorker, timeout: float = 10.0) -> None:
    """The line's own `done_event` fires a moment before its `release_queue()`
    (the loop still has to pop the run and notice the queue is empty), so a
    caller that just saw a terminal state polls this instead of asserting
    `queue_held is False` in the same instant.
    """
    deadline = time.monotonic() + timeout
    while worker.queue_held and time.monotonic() < deadline:
        time.sleep(0.02)
    assert worker.queue_held is False, "the queue was never released"


class TestTheSample:
    def test_pages_are_spread_across_series_and_volumes(self, storage: Path) -> None:
        """A benchmark over 32 consecutive pages of one book measures that book."""
        _library(
            storage,
            Alpha={"Volume 1": 40, "Volume 2": 40},
            Beta={"Volume 1": 40},
        )
        sample = build_sample(storage, 12)
        try:
            names = sorted(p.name for p in sample.directory.iterdir())
            assert sample.pages == len(names) == 12
            assert sample.volumes == 3
            # Four from each of the three volumes.
            stems = [name.split("_", 1)[1].rsplit(".", 1)[0] for name in names]
            assert sorted(set(stems)) == ["Volume_1", "Volume_2"]
            assert all(name.endswith(".jpg") for name in names), "no thumbnails sampled"
        finally:
            sample.cleanup()
        assert not sample.directory.exists()

    @pytest.mark.parametrize("total", [3, 4, 5, 10, 40, 200])
    def test_one_page_of_a_volume_is_never_its_cover(self, total: int) -> None:
        """`_spread(1, n)` was ``[0]``: with 32+ archives every sample page
        was a cover (6.0 text lines against 14.4 in the library; the bench
        read 2.6-3.4x faster than the same row live)."""
        from mokuro_bunko.ocr.bench import _spread

        (only,) = _spread(1, total)
        assert 0 < only < total - 1

    def test_spread_takes_midpoints_and_skips_the_ends(self) -> None:
        from mokuro_bunko.ocr.bench import _spread

        assert _spread(4, 200) == sorted(set(_spread(4, 200)))
        picked = _spread(4, 200)
        assert len(picked) == 4
        assert min(picked) >= 2 and max(picked) <= 197
        # Evenly through the interior, not bunched at the front.
        assert picked[0] > 10 and picked[-1] > 150
        assert _spread(2, 2) == [0, 1]
        assert _spread(1, 1) == [0]
        assert _spread(5, 3) == [0, 1, 2]

    def test_a_big_library_samples_interiors_of_a_few_volumes(self, storage: Path) -> None:
        """40 volumes, each opening on a cover: the sample holds no page 0,
        and reads several pages from each of a few volumes."""
        for index in range(40):
            path = storage / "library" / f"Series {index:02d}" / "Volume 1.cbz"
            path.parent.mkdir(parents=True, exist_ok=True)
            with zipfile.ZipFile(path, "w") as zf:
                for n in range(30):
                    zf.writestr(f"page_{n:03d}.jpg", f"{index}:{n}".encode())
        sample = build_sample(storage, 32)
        try:
            pages = [p.read_bytes().decode() for p in sample.directory.iterdir()]
            assert sample.pages == len(pages) == 32
            assert not any(page.endswith(":0") for page in pages), "a cover was sampled"
            assert not any(page.endswith(":29") for page in pages), "a back page was sampled"
            assert sample.volumes == 8
            per_volume = {}
            for page in pages:
                per_volume.setdefault(page.split(":")[0], []).append(page)
            assert {len(v) for v in per_volume.values()} == {4}
        finally:
            sample.cleanup()

    def test_short_volumes_top_the_sample_up_from_more_volumes(self, storage: Path) -> None:
        _library(storage, **{f"S{index}": {"Volume 1": 3} for index in range(12)})
        sample = build_sample(storage, 16)
        try:
            assert sample.pages == 16
            assert sample.volumes > 4
        finally:
            sample.cleanup()

    def test_an_empty_library_is_a_clear_400(self, storage: Path) -> None:
        with pytest.raises(BenchError) as caught:
            build_sample(storage, 32)
        assert caught.value.status == 400
        assert "no volumes in the library" in caught.value.message

    def test_the_sample_lives_under_processing_and_is_removed(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 10})
        sample = build_sample(storage, 4)
        assert sample.directory.parent == storage / ".processing"
        sample.cleanup()
        assert list((storage / ".processing").iterdir()) == []


class TestTheHost:
    def test_the_cpu_carries_its_core_count_and_a_missing_gpu_is_null(self) -> None:
        with patch("mokuro_bunko.ocr.bench._run_probe", return_value=None):
            host = describe_host("rocm", Path("/nonexistent"))
        assert host["backend"] == "rocm"
        assert host["gpu"] is None
        assert isinstance(host["cpu"], str) and "core" in host["cpu"]

    def test_the_gpu_is_asked_of_the_engines_environment_first(self) -> None:
        """The card the OCR really runs on, not whatever the host also has."""
        calls: list[list[str]] = []

        def probe(command: Any, timeout: float) -> str | None:
            calls.append(list(command))
            return "AMD Radeon RX 9070 XT" if "-c" in command else "some other card"

        with patch("mokuro_bunko.ocr.bench._run_probe", side_effect=probe):
            host = describe_host("rocm", Path("/opt/engines/bin/python"))
        assert host["gpu"] == "AMD Radeon RX 9070 XT"
        assert calls[0][0] == "/opt/engines/bin/python"

    def test_rocm_smi_s_card_series_is_the_name_not_its_csv_header(self) -> None:
        """`rocm-smi --showproductname --csv` opens with a header row; the
        name is the Card Series column of the first card's row."""
        csv = (
            "device,Card Series,Card Model,Card Vendor,Card SKU\n"
            "card0,AMD Radeon RX 9070 XT,0x7550,Advanced Micro Devices Inc. [AMD/ATI],APM\n"
        )

        def run(command: Any, **_kwargs: Any) -> subprocess.CompletedProcess[str]:
            if command[0] == "rocm-smi":
                return subprocess.CompletedProcess(command, 0, csv, "")
            raise FileNotFoundError(command[0])

        with patch("mokuro_bunko.ocr.bench.subprocess.run", side_effect=run):
            host = describe_host("rocm", None)
        assert host["gpu"] == "AMD Radeon RX 9070 XT"


class TestTheComposedBenchmark:
    def test_a_full_run_composes_the_runner_events(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20}, Beta={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage))
        service = _service(storage, rows, worker, remaining_pages=lambda row: 5230)

        queued = service.enqueue(rows[1].id, None, 8)
        assert queued["state"] in ("queued", "running")
        assert queued["key"] == rows[1].id
        assert queued["generation"] == rows[1].id
        assert queued["spec"]["engine"] == "hayai-nova"

        data = _wait_for(service, rows[1].id, "done")
        assert data["waiting_for_queue"] is False
        assert data["tunable"] is True
        assert data["sample"] == {"pages": 8, "volumes": 2}
        assert data["host"]["backend"] == "rocm"
        assert data["startup_seconds"] == 11.2
        assert [trial["n"] for trial in data["trials"]] == [1, 2]
        assert data["trials"][0]["note"] == "auto"
        assert data["trials"][0]["bottleneck"] == "detect"
        assert data["baseline"] == {"pages_per_second": 1.5, "seconds_per_page": 0.667}
        assert data["best"]["stage_workers"] == {"detect": 3}
        assert data["best"]["speedup"] == 1.4
        assert data["peak_rss_mb"] == 2410
        assert data["peak_vram_mb"] == 3120
        # `progress` is only ever present while it runs.
        assert data["progress"] is None
        assert data["started_at"] and data["finished_at"]
        assert data["error"] is None
        assert data["preempted"] == []
        # The FIFO is global and this run has to leave it: read it once the
        # line has actually let the OCR queue go, not in the same instant the
        # result became visible (the line still has to pop its own head).
        _wait_until_released(worker)
        assert service.get(rows[1].id)["queue"] == {"running": None, "queued": []}

    def test_each_trial_carries_the_utilization_over_ITS_window(
        self, storage: Path
    ) -> None:
        """ADDENDUM 9: "neither the GPU nor the CPU seemed tapped" as a number.

        The runner reports each trial's window in seconds since its own
        process start; the server turns that into a pair of means over the
        samples it took inside exactly those seconds -- never over the model
        load in front of them, and never over another trial's.
        """
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage))
        service = _service(storage, rows, worker)
        asked: list[tuple[float, float]] = []

        class _Sampler:
            def start(self) -> Any:
                return self

            def stop(self, **_kw: Any) -> None:
                pass

            def means(self, first: float, last: float) -> dict[str, float]:
                asked.append((first, last))
                return {"gpu_busy_pct": 36.0 + len(asked), "cpu_busy_pct": 71.5}

        with patch.object(BenchService, "_sampler", lambda self, device: _Sampler()):
            service.enqueue(rows[1].id, None, 8)
            data = _wait_for(service, rows[1].id, "done")

        assert [t["gpu_busy_pct"] for t in data["trials"]] == [37.0, 38.0]
        assert [t["cpu_busy_pct"] for t in data["trials"]] == [71.5, 71.5]
        # The windows asked for are the trials' own (20 s and 21 s wide), and
        # both are placed after the same spawn instant.
        assert [round(last - first, 3) for first, last in asked] == [20.0, 21.0]
        assert asked[1][0] - asked[0][0] == pytest.approx(22.0)
        # The headline shows the winning trial's numbers, and its utilization.
        assert data["best"]["gpu_busy_pct"] == 38.0
        assert data["best"]["window_seconds"] == 21.0
        assert data["best"]["short_window"] is False

    def test_a_zero_peak_vram_is_a_fact_about_the_process_not_a_failure(
        self, storage: Path
    ) -> None:
        """A served engine holds its model in ANOTHER process (ADDENDUM 8).

        The runner then allocates nothing itself -- it may not even import
        torch -- so `peak_vram_mb` is 0 while the card is fully busy. That
        must pass through as the number it is: not an error, not a retry,
        and not something the utilization sampler is asked about instead.
        """
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        script = _script(storage, bench={"peak_vram_mb": 0, "peak_rss_mb": 0})
        worker = _worker(storage, rows, script)
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        data = _wait_for(service, rows[1].id, "done")
        assert data["error"] is None
        assert data["peak_vram_mb"] == 0
        assert data["peak_rss_mb"] == 0
        # ... and everything that DOES carry on that road is still there.
        assert data["best"]["pages_per_second"] == 2.1
        assert data["trials"][0]["window_seconds"] == 20.0
        assert service.saved(rows[1].id) is not None

    def test_a_trial_the_runner_could_not_window_gets_no_utilization(
        self, storage: Path
    ) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        script = _script(
            storage,
            bench={
                "trials": [
                    {
                        "n": 1,
                        "note": "auto",
                        "stage_workers": {"detect": 1},
                        "queue_capacity": {"detect": 1},
                        "seconds": 6.0,
                        "pages_per_second": 0.0,
                        "window_seconds": 0.0,
                        "pages_measured": 6,
                        "passes": 8,
                        "short_window": True,
                        "accepted": True,
                        "verdict": None,
                        "bottleneck": None,
                        "stages": [],
                        "queues": [],
                    }
                ]
            },
        )
        worker = _worker(storage, rows, script)
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        data = _wait_for(service, rows[1].id, "done")
        trial = data["trials"][0]
        assert trial["short_window"] is True
        assert trial["gpu_busy_pct"] is None and trial["cpu_busy_pct"] is None

    def test_a_spec_measures_a_hypothetical_row_that_was_never_saved(
        self, storage: Path
    ) -> None:
        """A draft key's result is held in memory only -- never `.ocr-bench.json`."""
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY)  # hayai-nova is NOT a saved row here
        worker = _worker(storage, rows, _script(storage))
        service = _service(storage, rows, worker)

        result = service.enqueue("draft-hayai", dict(HAYAI_SPEC), 8)
        assert result["key"] == "draft-hayai"
        assert result["spec"]["engine"] == "hayai-nova"

        data = _wait_for(service, "draft-hayai", "done")
        assert data["best"]["pages_per_second"] == 2.1
        assert not (storage / ".ocr-bench.json").exists()
        # Readable while the process lives...
        assert service.get("draft-hayai")["state"] == "done"
        # ...gone for a fresh service standing in for a restart.
        fresh = _service(storage, rows, worker)
        assert fresh.get("draft-hayai") == {
            "state": "idle",
            "generation": "draft-hayai",
            "key": "draft-hayai",
            "queue": {"running": None, "queued": []},
        }

    def test_a_spec_for_a_saved_id_overrides_what_is_measured_and_still_persists(
        self, storage: Path
    ) -> None:
        """Posting a spec for an existing id measures the SPEC, not the saved pools."""
        _library(storage, Alpha={"Volume 1": 20})
        tuned = {**HAYAI, "pools": {"stage_workers": {"detect": 7}}}
        rows = _gens(PRIMARY, tuned)
        worker = _worker(storage, rows, _script(storage))
        service = _service(storage, rows, worker)

        service.enqueue(rows[1].id, dict(HAYAI_SPEC), 8)  # untuned spec
        data = _wait_for(service, rows[1].id, "done")
        assert data["spec"]["engine"] == "hayai-nova"
        assert data["spec"]["pools"] == {
            "stage_workers": {},
            "queue_capacity": {},
            "stage_device": {},
        }
        # best={"detect": 3} (fake runner) != the spec's {} pools.
        assert data["best"]["same_as_spec"] is False
        assert service.saved(rows[1].id) is not None, "a saved id's result still persists"

    def test_the_estimates_are_this_row_s_own(self, storage: Path) -> None:
        """200/pps, and what is left of the queue at that rate. NO startup.

        ADDENDUM 9: the model load is paid once per SESSION, not per volume,
        and a rate it was added to would say a fast engine with a slow load
        is a slow engine. It is reported once, separately, as
        `startup_seconds` -- which this run still carries.
        """
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage))
        service = _service(storage, rows, worker, remaining_pages=lambda row: 5230)
        service.enqueue(rows[1].id, None, 8)
        data = _wait_for(service, rows[1].id, "done")
        assert data["estimates"] == {
            # 200 pages / 2.1 pages a second, and nothing else
            "volume_200_pages_seconds": 95,
            "remaining_pages": 5230,
            "remaining_seconds": 2490,
        }
        assert data["startup_seconds"] == 11.2, "still reported, just never added"

    def test_remaining_is_null_when_the_server_does_not_know(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        data = _wait_for(service, rows[1].id, "done")
        assert data["estimates"]["remaining_pages"] is None
        assert data["estimates"]["remaining_seconds"] is None
        assert data["estimates"]["volume_200_pages_seconds"] == 95

    def test_same_as_spec_compares_the_best_widths_to_the_measured_spec(
        self, storage: Path
    ) -> None:
        """`{}` means auto is already best; a match means there is nothing to apply."""
        _library(storage, Alpha={"Volume 1": 20})
        tuned = {**HAYAI, "pools": {"stage_workers": {"detect": 3}}}
        rows = _gens(PRIMARY, tuned)
        worker = _worker(storage, rows, _script(storage))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        assert _wait_for(service, rows[1].id, "done")["best"]["same_as_spec"] is True

        untuned = _gens(PRIMARY, HAYAI)
        worker2 = _worker(storage, untuned, _script(storage))
        service2 = _service(storage, untuned, worker2)
        service2.enqueue(untuned[1].id, None, 8)
        assert _wait_for(service2, untuned[1].id, "done")["best"]["same_as_spec"] is False

    def test_a_fatal_runner_is_a_failed_benchmark_with_its_reason(
        self, storage: Path
    ) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage, bench={"fatal": "no GPU found"}))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        data = _wait_for(service, rows[1].id, "failed")
        assert "no GPU found" in data["error"]
        assert service.saved(rows[1].id) is None, "a failure is not a result to keep"

    def test_a_running_benchmark_is_cancellable(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage, bench={"hang": True}))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        _wait_for(service, rows[1].id, "running")
        data = service.cancel(rows[1].id)
        assert data["state"] == "cancelled"
        assert data["finished_at"]
        assert service.saved(rows[1].id) is None
        _wait_until_released(worker)

    def test_progress_is_present_only_while_running(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        script = _script(storage, bench={"trial_delay": 0.4})
        worker = _worker(storage, rows, script)
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        seen: dict[str, Any] | None = None
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            data = service.get(rows[1].id)
            if data.get("state") == "running" and data.get("progress"):
                seen = data["progress"]
                break
            if data.get("state") == "done":
                break
            time.sleep(0.02)
        assert seen is not None
        assert set(seen) >= {"trial", "max_trials", "pages_done", "pages", "stage_workers"}
        assert _wait_for(service, rows[1].id, "done")["progress"] is None


def _writes_page_json(pages: int, *, first_at: float, spacing: float) -> Any:
    """A `_run_mokuro` stand-in that writes what the real one writes.

    The fork writes one JSON under ``<workspace>/_ocr/<volume>/`` as each
    page comes back out of its pipeline, and ADDENDUM 9 times the monolithic
    road by exactly those mtimes. So the fake controls them: the first lands
    ``first_at`` seconds after the run starts (the model load), and the rest
    ``spacing`` apart.
    """

    def run(input_path: Path, output_dir: Path, **_kw: Any) -> MokuroRunResult:
        root = Path(output_dir) / "_ocr" / Path(input_path).name
        root.mkdir(parents=True, exist_ok=True)
        base = time.time()
        for index in range(pages):
            path = root / f"page_{index:03d}.json"
            path.write_text("{}", encoding="utf-8")
            when = int((base + first_at + index * spacing) * 1e9)
            os.utime(path, ns=(when, when))
        return MokuroRunResult(True, None, None)

    return run


class TestTheMonolithicBenchmark:
    """ADDENDUM 9: the mokuro CLI is timed by the JSON files it emits.

    One stopwatch around the whole process reported 1.0 pages/s for an engine
    that reads ~13 pages/s, because more than half of the 28 s it measured
    was the interpreter, the torch import, the model load and the worker
    spawn. None of that is between two page emissions, so none of it is in
    these numbers.
    """

    def test_the_rate_comes_from_the_json_mtimes_not_a_stopwatch(
        self, storage: Path
    ) -> None:
        _library(storage, Alpha={"Volume 1": 40})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows)
        # 32 pages: 14 s of model load, then a page every half second.
        with patch.object(
            worker.processor,
            "_run_mokuro",
            side_effect=_writes_page_json(32, first_at=14.0, spacing=0.5),
        ) as run:
            service = _service(storage, rows, worker, remaining_pages=lambda row: 1000)
            service.enqueue(rows[0].id, None, 32)
            data = _wait_for(service, rows[0].id, "done")
        assert run.call_count == 1
        trial = data["trials"][0]
        # fill = min(8, 32/4) = 8, so 24 emissions over 23 x 0.5 s.
        assert trial["pages_measured"] == 24
        assert trial["window_seconds"] == pytest.approx(11.5, abs=0.05)
        assert trial["pages_per_second"] == pytest.approx(2.0, abs=0.02)
        assert trial["passes"] == 1
        # The load is reported, once, and is in nothing else.
        assert data["startup_seconds"] == pytest.approx(14.0, abs=0.5)
        assert data["estimates"]["volume_200_pages_seconds"] == 100
        assert data["best"]["pages_per_second"] == pytest.approx(2.0, abs=0.02)

    def test_the_pages_are_given_to_mokuro_inside_the_workspace(
        self, storage: Path
    ) -> None:
        """Or its `_ocr` cache lands where nothing here can see it."""
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows)
        seen: list[int] = []
        write = _writes_page_json(8, first_at=1.0, spacing=1.0)

        def run_and_count(input_path: Path, output_dir: Path, **kw: Any) -> MokuroRunResult:
            seen.append(sum(1 for _ in Path(input_path).iterdir()))
            return write(input_path, output_dir, **kw)

        with patch.object(worker.processor, "_run_mokuro", side_effect=run_and_count) as run:
            service = _service(storage, rows, worker)
            service.enqueue(rows[0].id, None, 8)
            _wait_for(service, rows[0].id, "done")
        given = run.call_args.args[0]
        workspace = run.call_args.args[1]
        assert given.parent == workspace, "mokuro writes _ocr beside what it is given"
        assert workspace.parent == storage / ".processing"
        assert seen == [8], "and it is handed the whole sample, not a link to it"
        # ...and nothing of it survives. Checked once the line has let the
        # queue go: the result is visible a moment before the workspace is
        # swept, and this is about the sweep.
        _wait_until_released(worker)
        assert not workspace.exists()
        assert not any((storage / ".processing").glob("_ocr"))

    def test_one_run_and_nothing_to_tune(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows)
        with patch.object(
            worker.processor,
            "_run_mokuro",
            side_effect=_writes_page_json(8, first_at=2.0, spacing=2.0),
        ):
            service = _service(storage, rows, worker)
            service.enqueue(rows[0].id, None, 8)
            data = _wait_for(service, rows[0].id, "done")
        assert data["tunable"] is False
        assert len(data["trials"]) == 1
        assert data["trials"][0]["stages"] == []
        assert data["best"]["speedup"] == 1.0
        assert data["best"]["stage_workers"] == {}
        assert data["best"]["same_as_spec"] is True

    def test_a_sample_that_lands_in_one_burst_is_flagged_short(
        self, storage: Path
    ) -> None:
        """The honest answer for a volume that fits inside mokuro's pipeline.

        It cannot be fixed by looping (a second pass pays the model load
        again); the served road is the fix. What must NOT happen is the six
        figures a collapsed window produced.
        """
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows)
        with patch.object(
            worker.processor,
            "_run_mokuro",
            side_effect=_writes_page_json(8, first_at=12.0, spacing=5e-6),
        ):
            service = _service(storage, rows, worker)
            service.enqueue(rows[0].id, None, 8)
            data = _wait_for(service, rows[0].id, "done")
        trial = data["trials"][0]
        assert trial["short_window"] is True
        assert trial["window_seconds"] < 0.001
        assert trial["pages_per_second"] < 1e6

    def test_a_run_that_wrote_nothing_cannot_be_timed_and_says_so(
        self, storage: Path
    ) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows)
        with patch.object(
            worker.processor, "_run_mokuro", return_value=MokuroRunResult(True, None, None)
        ):
            service = _service(storage, rows, worker)
            service.enqueue(rows[0].id, None, 8)
            data = _wait_for(service, rows[0].id, "failed")
        assert "nothing to measure" in data["error"]

    def test_a_failed_mokuro_run_is_a_failed_benchmark(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows)
        with patch.object(
            worker.processor,
            "_run_mokuro",
            return_value=MokuroRunResult(False, "no module named mokuro", None),
        ):
            service = _service(storage, rows, worker)
            service.enqueue(rows[0].id, None, 8)
            data = _wait_for(service, rows[0].id, "failed")
        assert data["error"] == "no module named mokuro"

    def test_a_monolithic_spec_needs_only_the_engine(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY)
        worker = _worker(storage, rows)
        with patch.object(
            worker.processor,
            "_run_mokuro",
            side_effect=_writes_page_json(8, first_at=1.0, spacing=1.5),
        ):
            service = _service(storage, rows, worker)
            service.enqueue("draft-mono", {"engine": "mokuro"}, 8)
            data = _wait_for(service, "draft-mono", "done")
        assert data["tunable"] is False
        assert data["best"]["speedup"] == 1.0
        assert data["spec"] == {
            "engine": "mokuro",
            "patch_budget": 512,
            "pools": {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}},
        }


class TestExclusivity:
    def test_the_queue_is_held_while_it_runs_and_let_go_after(
        self, storage: Path
    ) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage, bench={"trial_delay": 0.5}))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        _wait_for(service, rows[1].id, "running")
        assert worker.queue_held is True
        assert worker.claim_next() is None, "nothing is claimed beside a benchmark"
        _wait_for(service, rows[1].id, "done")
        _wait_until_released(worker)

    def test_preempted_volumes_ride_the_first_benchmark_of_a_line_only(
        self, storage: Path
    ) -> None:
        """ADDENDUM 5/6: pre-empted ONCE per line, not once per benchmark."""
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage))
        canned = [{"generation": "mokuro", "volume": "Volume 3"}]
        calls: list[float] = []
        real_preempt = worker.preempt_for_bench

        def fake_preempt(
            timeout: float = 900.0, processor: str = "local"
        ) -> tuple[bool, list[dict[str, str]]]:
            calls.append(timeout)
            # exercise the real hold bookkeeping too
            real_preempt(timeout=0.01, processor=processor)
            return True, list(canned)

        worker.preempt_for_bench = fake_preempt  # type: ignore[method-assign]
        service = _service(storage, rows, worker)

        service.enqueue(rows[1].id, None, 8)
        data = _wait_for(service, rows[1].id, "done")
        assert data["preempted"] == canned
        assert len(calls) == 1, "pre-empted exactly once for the whole line"

        # A second benchmark, once the first line has fully drained, is a
        # NEW line and pre-empts again. "done" is visible before the line's
        # thread has let the machine go; enqueued in between, the second
        # would join the SAME line (rightly without a second pre-empt).
        _wait_until_released(worker)
        service.enqueue(rows[1].id, None, 8)
        _wait_for(service, rows[1].id, "done")
        assert len(calls) == 2

    def test_the_hold_spans_the_whole_line_not_each_benchmark(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage, bench={"trial_delay": 0.3}))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        service.enqueue("draft-2", dict(HAYAI_SPEC), 8)
        _wait_for(service, rows[1].id, "running")
        assert worker.queue_held is True
        _wait_for(service, rows[1].id, "done")
        # Still held: the second benchmark of the line is running or about to.
        assert worker.queue_held is True
        _wait_for(service, "draft-2", "done", timeout=30)
        _wait_until_released(worker)


class TestTheQueue:
    """ADDENDUM 6: benchmarks queue instead of refusing a second request."""

    def test_a_different_row_queues_instead_of_409(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage, bench={"hang": True}))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        _wait_for(service, rows[1].id, "running")

        second = service.enqueue(rows[0].id, None)
        assert second["state"] == "queued"
        assert second["position"] == 1
        assert second["queue"] == {"running": rows[1].id, "queued": [rows[0].id]}

        service.cancel(rows[0].id)
        service.cancel(rows[1].id)

    def test_the_same_key_twice_is_409(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage, bench={"hang": True}))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        _wait_for(service, rows[1].id, "running")
        with pytest.raises(BenchError) as caught:
            service.enqueue(rows[1].id, None)
        assert caught.value.status == 409
        assert "hayai-nova" in caught.value.message
        service.cancel(rows[1].id)

    def test_three_posts_run_strictly_in_the_order_they_were_queued(
        self, storage: Path
    ) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage, bench={"trial_delay": 0.2}))
        service = _service(storage, rows, worker)
        keys = [rows[1].id, "draft-b", "draft-c"]

        first = service.enqueue(keys[0], None, 8)
        second = service.enqueue(keys[1], dict(HAYAI_SPEC), 8)
        third = service.enqueue(keys[2], dict(HAYAI_SPEC), 8)
        assert (first["position"], second["position"], third["position"]) == (0, 1, 2)
        assert first["state"] in ("queued", "running")
        assert second["state"] == "queued"
        assert third["state"] == "queued"

        started_order: list[str] = []
        deadline = time.monotonic() + 30
        while len(started_order) < 3 and time.monotonic() < deadline:
            for key in keys:
                if key not in started_order and service.get(key)["state"] == "running":
                    started_order.append(key)
            time.sleep(0.02)
        assert started_order == keys, "strict FIFO: run in the order they were posted"
        for key in keys:
            _wait_for(service, key, "done", timeout=30)

    def test_delete_of_a_queued_one_shifts_the_positions_behind_it(
        self, storage: Path
    ) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage, bench={"hang": True}))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        _wait_for(service, rows[1].id, "running")
        service.enqueue("draft-b", dict(HAYAI_SPEC), 8)
        third = service.enqueue("draft-c", dict(HAYAI_SPEC), 8)
        assert third["position"] == 2

        cancelled = service.cancel("draft-b")
        assert cancelled["state"] == "cancelled"
        assert service.get("draft-c")["position"] == 1

        service.cancel("draft-c")
        service.cancel(rows[1].id)

    def test_delete_of_the_running_one_starts_the_next(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage, bench={"hang": True}))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        _wait_for(service, rows[1].id, "running")
        service.enqueue("draft-b", dict(HAYAI_SPEC), 8)

        cancelled = service.cancel(rows[1].id)
        assert cancelled["state"] == "cancelled"
        _wait_for(service, "draft-b", "running", timeout=20)
        service.cancel("draft-b")

    def test_a_failing_benchmark_does_not_stall_the_line(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage, bench={"fatal": "no GPU found"}))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        second = service.enqueue("draft-b", dict(HAYAI_SPEC), 8)
        assert second["position"] == 1
        _wait_for(service, rows[1].id, "failed")
        _wait_for(service, "draft-b", "failed", timeout=30)
        _wait_until_released(worker)


class TestPausedForBenchmark:
    def test_the_shape_while_a_line_holds_the_queue(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage, bench={"hang": True}))
        service = _service(storage, rows, worker)
        assert service.paused_for_benchmark() is None

        service.enqueue(rows[1].id, None, 8)
        _wait_for(service, rows[1].id, "running")
        service.enqueue("draft-b", dict(HAYAI_SPEC), 8)
        assert service.paused_for_benchmark() == {
            "key": rows[1].id,
            "generation": "hayai-nova",
            "queued": 1,
            "processor": "local",
        }

        service.cancel("draft-b")
        service.cancel(rows[1].id)
        deadline = time.monotonic() + 10
        while service.paused_for_benchmark() is not None and time.monotonic() < deadline:
            time.sleep(0.02)
        assert service.paused_for_benchmark() is None

    def test_a_draft_key_shows_as_its_own_generation_name(self, storage: Path) -> None:
        """No saved row backs a draft key, so it IS the display name."""
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY)
        worker = _worker(storage, rows, _script(storage, bench={"hang": True}))
        service = _service(storage, rows, worker)
        service.enqueue("draft-x", dict(HAYAI_SPEC), 8)
        _wait_for(service, "draft-x", "running")
        assert service.paused_for_benchmark() == {
            "key": "draft-x",
            "generation": "draft-x",
            "queued": 0,
            "processor": "local",
        }
        service.cancel("draft-x")


class TestRequests:
    @pytest.mark.parametrize(
        "case,message",
        [
            ("unknown", "no generation"),
            ("pages", "whole number between"),
            ("environment", "not installed"),
            ("no-worker", "OCR is disabled"),
        ],
    )
    def test_the_400s_say_what_is_wrong(
        self, storage: Path, case: str, message: str
    ) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows)
        problem = (
            (lambda row: "the OCR engines environment is not installed yet")
            if case == "environment"
            else (lambda row: None)
        )
        service = _service(
            storage,
            rows,
            None if case == "no-worker" else worker,
            environment_problem=problem,
        )
        with pytest.raises(BenchError) as caught:
            if case == "unknown":
                service.enqueue("g-99", None)
            elif case == "pages":
                service.enqueue(rows[1].id, None, 2)
            else:
                service.enqueue(rows[1].id, None)
        assert caught.value.status == 400
        assert message in caught.value.message

    def test_a_disabled_row_can_still_be_benchmarked(self, storage: Path) -> None:
        """A benchmark answers "should I enable this?" -- it must work on one that isn't."""
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, {**HAYAI, "enabled": False})
        worker = _worker(storage, rows, _script(storage))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        data = _wait_for(service, rows[1].id, "done")
        assert data["error"] is None

    def test_spec_validation_is_a_400_with_row_none_and_the_field(
        self, storage: Path
    ) -> None:
        rows = _gens(PRIMARY)
        service = _service(storage, rows, _worker(storage, rows))
        with pytest.raises(BenchError) as caught:
            service.enqueue("draft-x", {"engine": "not-a-real-engine"})
        assert caught.value.status == 400
        assert caught.value.row is None
        assert caught.value.field == "engine"

    def test_a_pools_key_off_the_road_is_a_400_with_its_field(self, storage: Path) -> None:
        rows = _gens(PRIMARY)
        service = _service(storage, rows, _worker(storage, rows))
        with pytest.raises(BenchError) as caught:
            service.enqueue(
                "draft-x",
                {"engine": "hayai-nova", "pools": {"stage_workers": {"not-a-stage": 2}}},
            )
        assert caught.value.status == 400
        assert caught.value.row is None
        assert caught.value.field == "pools"

    def test_spec_absent_and_an_unknown_key_is_a_400(self, storage: Path) -> None:
        rows = _gens(PRIMARY)
        service = _service(storage, rows, _worker(storage, rows))
        with pytest.raises(BenchError) as caught:
            service.enqueue("g-99", None)
        assert caught.value.status == 400
        assert "no generation" in caught.value.message

    def test_a_key_that_is_neither_a_draft_nor_a_saved_id_is_a_400_even_with_a_spec(
        self, storage: Path
    ) -> None:
        rows = _gens(PRIMARY)
        service = _service(storage, rows, _worker(storage, rows))
        with pytest.raises(BenchError) as caught:
            service.enqueue("not-a-draft-key", {"engine": "hayai-nova"})
        assert caught.value.status == 400

    def test_an_empty_library_is_refused_before_anything_is_held(
        self, storage: Path
    ) -> None:
        """Said at once, not discovered after a model load and a queue hold."""
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage))
        service = _service(storage, rows, worker)
        with pytest.raises(BenchError) as caught:
            service.enqueue(rows[1].id, None, 8)
        assert caught.value.status == 400
        assert "no volumes in the library" in caught.value.message
        assert worker.queue_held is False

    def test_cancelling_nothing_is_a_400(self, storage: Path) -> None:
        rows = _gens(PRIMARY, HAYAI)
        service = _service(storage, rows, _worker(storage, rows))
        with pytest.raises(BenchError) as caught:
            service.cancel(rows[1].id)
        assert caught.value.status == 400

    def test_a_row_that_never_ran_reads_as_idle(self, storage: Path) -> None:
        rows = _gens(PRIMARY, HAYAI)
        service = _service(storage, rows, _worker(storage, rows))
        assert service.get(rows[1].id) == {
            "state": "idle",
            "generation": rows[1].id,
            "key": rows[1].id,
            "queue": {"running": None, "queued": []},
        }


class TestPersistence:
    def test_the_last_finished_result_is_kept_and_pruned(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        _wait_for(service, rows[1].id, "done")

        saved = json.loads((storage / ".ocr-bench.json").read_text(encoding="utf-8"))
        assert list(saved) == [rows[1].id]
        assert saved[rows[1].id]["best"]["pages_per_second"] == 2.1
        # A later GET of a row with no live run reads the saved one back.
        assert service.get(rows[1].id)["state"] == "done"

        service.prune([rows[0].id])
        assert not (storage / ".ocr-bench.json").exists()

    def test_the_table_copy_omits_the_trials(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        _wait_for(service, rows[1].id, "done")

        summaries = service.saved_summaries(rows)
        assert list(summaries) == [rows[1].id]
        assert "trials" not in summaries[rows[1].id]
        assert summaries[rows[1].id]["best"]["pages_per_second"] == 2.1
        assert summaries[rows[1].id]["progress"] is None

    def test_a_result_survives_a_new_service(self, storage: Path) -> None:
        _library(storage, Alpha={"Volume 1": 20})
        rows = _gens(PRIMARY, HAYAI)
        worker = _worker(storage, rows, _script(storage))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, 8)
        _wait_for(service, rows[1].id, "done")
        fresh = _service(storage, rows, worker)
        assert fresh.get(rows[1].id)["best"]["speedup"] == 1.4


def test_the_bench_command_starts_from_the_derived_widths(storage: Path) -> None:
    """A benchmark measures the MACHINE; the row's tuning is what it questions."""
    from mokuro_bunko.ocr.processor import OCRProcessor

    rows = _gens(PRIMARY, {**HAYAI, "pools": {"stage_workers": {"detect": 4}}})
    proc = OCRProcessor(
        storage_path=storage, generations=rows, engines_python_path=Path("/usr/bin/python3")
    )
    session = proc.open_bench(rows[1], storage / "sample", storage / "bench.log")
    cmd = session.command
    assert cmd[2] == "--bench"
    args = dict(zip(cmd[3::2], cmd[4::2], strict=True))
    assert args["--engine"] == "hayai-nova"
    assert args["--input"] == str(storage / "sample")
    assert args["--session-log"] == str(storage / "bench.log")
    assert args["--bench-max-trials"] == "8"
    assert args["--bench-budget-seconds"] == "900"
    assert "--stage-workers" not in cmd
    assert "--queue-capacity" not in cmd


def test_the_host_names_the_devices_the_run_used(storage: Path) -> None:
    """A number measured on the CPU is about a different machine (Addendum 7)."""
    _library(storage, Alpha={"Volume 1": 20})
    rows = _gens(PRIMARY, HAYAI)
    worker = _worker(
        storage,
        rows,
        _script(storage, bench={"stage_device": {"detect": "cpu", "engine": "gpu:0"}}),
    )
    service = _service(storage, rows, worker)
    service.enqueue(rows[1].id, None, None)
    data = _wait_for(service, rows[1].id, "done")
    assert data["host"]["devices"] == {"detect": "cpu", "engine": "gpu:0"}


def test_applying_a_placement_is_a_change_like_any_other(storage: Path) -> None:
    """`same_as_spec` is false while the spec is still on the other device."""
    _library(storage, Alpha={"Volume 1": 20})
    rows = _gens(PRIMARY, HAYAI)
    worker = _worker(
        storage,
        rows,
        _script(
            storage,
            bench={"best": {"trial": 2, "stage_workers": {}, "queue_capacity": {},
                            "stage_device": {"detect": "cpu"}, "pages_per_second": 2.1,
                            "seconds_per_page": 0.476, "speedup": 1.4}},
        ),
    )
    service = _service(storage, rows, worker)
    service.enqueue(rows[1].id, None, None)
    data = _wait_for(service, rows[1].id, "done")
    assert data["best"]["stage_device"] == {"detect": "cpu"}
    assert data["best"]["same_as_spec"] is False


def test_the_bench_command_measures_the_placement_it_was_given(storage: Path) -> None:
    """The widths are the question; the DEVICE is part of the spec asked about."""
    from mokuro_bunko.ocr.processor import OCRProcessor

    rows = _gens(PRIMARY, {**HAYAI, "pools": {"stage_device": {"detect": "cpu"}}})
    proc = OCRProcessor(
        storage_path=storage, generations=rows, engines_python_path=Path("/usr/bin/python3")
    )
    cmd = proc.open_bench(rows[1], storage / "sample", storage / "bench.log").command
    assert dict(zip(cmd[3::2], cmd[4::2], strict=True))["--stage-device"] == "detect=cpu"


# --- best.stage_device is a DELTA against the placement it was measured with ---
#
# The runner's ``--bench`` is given the spec's ``--stage-device`` (the placement
# is part of the question) but not its widths (those are what it questions), so
# its ``best.stage_workers`` is a whole table while its ``best.stage_device``
# names only the stages the search MOVED. Read as a whole table, a pin the run
# was measured with vanished from what was applied: the paddle-manga-animetext
# incident, where ``detect: cpu`` became ``auto`` and ``auto`` became a card the
# detector's onnxruntime could not reach.

UNMOVED = {"trial": 1, "stage_workers": {}, "queue_capacity": {}, "stage_device": {},
           "pages_per_second": 1.1, "seconds_per_page": 0.909, "speedup": 1.0}


def test_the_best_placement_keeps_the_pins_it_was_measured_with(storage: Path) -> None:
    _library(storage, Alpha={"Volume 1": 20})
    rows = _gens(PRIMARY, {**HAYAI, "pools": {"stage_device": {"detect": "cpu"}}})
    worker = _worker(storage, rows, _script(storage))  # best: detect x3, nothing moved
    service = _service(storage, rows, worker)
    service.enqueue(rows[1].id, None, None)
    data = _wait_for(service, rows[1].id, "done")
    assert data["best"]["stage_workers"] == {"detect": 3}
    assert data["best"]["stage_device"] == {"detect": "cpu"}, "applying it keeps the pin"


def test_a_best_that_moved_nothing_is_the_spec(storage: Path) -> None:
    _library(storage, Alpha={"Volume 1": 20})
    rows = _gens(PRIMARY, {**HAYAI, "pools": {"stage_device": {"detect": "cpu"}}})
    worker = _worker(storage, rows, _script(storage, bench={"best": dict(UNMOVED)}))
    service = _service(storage, rows, worker)
    service.enqueue(rows[1].id, None, None)
    data = _wait_for(service, rows[1].id, "done")
    assert data["best"]["stage_device"] == {"detect": "cpu"}
    assert data["best"]["same_as_spec"] is True


def test_a_pin_the_runner_did_not_honour_is_not_what_was_measured(storage: Path) -> None:
    """The runner put ``detect`` on the CPU although the spec pinned card 0
    (its onnxruntime cannot reach it): the number is about the CPU, and so is
    what applying it would set."""
    _library(storage, Alpha={"Volume 1": 20})
    rows = _gens(PRIMARY, {**HAYAI, "detector": "ctd", "pools": {"stage_device": {"detect": "gpu:0"}}})
    worker = _worker(
        storage,
        rows,
        _script(
            storage,
            bench={"best": dict(UNMOVED),
                   "stage_device": {"detect": "cpu", "engine": "gpu:0"}},
        ),
    )
    service = _service(storage, rows, worker)
    service.enqueue(rows[1].id, None, None)
    data = _wait_for(service, rows[1].id, "done")
    assert data["best"]["stage_device"] == {"detect": "cpu"}
    assert data["best"]["same_as_spec"] is False


# --- best.queue_capacity: the search never chooses one ---
#
# The runner's ``best.queue_capacity`` is always ``{}``: the search derives the
# capacities from the widths and never picks one. Read as a whole table, a
# row's queue pins (the paddle-manga-animetext row's ``{engine: 4, post: 4}``)
# vanished from what applying the result would set, and ``same_as_spec`` could
# never be true for that row, so an auto-benchmark that changed nothing still
# wrote a table without them into the machine's profile.
#
# Nor is the runner ever GIVEN those pins (``open_bench`` sends no
# ``--queue-capacity``), so every trial ran DERIVED capacities. A pin is what
# was measured only where the winning trial's derived capacity landed on it;
# anywhere else the benchmark measured ``auto`` there, and that is what the
# result says.

QUEUED = {**HAYAI, "pools": {"queue_capacity": {"engine": 4, "post": 4},
                             "stage_device": {"detect": "cpu"}}}


def _trial_at(capacity: dict[str, int], **over: Any) -> dict[str, Any]:
    """One accepted trial that ran at ``capacity``."""
    return {
        "n": 1, "note": "auto", "stage_workers": {"detect": 1, "engine": 1},
        "queue_capacity": dict(capacity), "seconds": 20.0, "pages_per_second": 1.1,
        "window_seconds": 20.0, "pages_measured": 22, "passes": 1,
        "short_window": False, "first_emission_at": 1.0, "last_emission_at": 21.0,
        "accepted": True, "verdict": None, "bottleneck": None,
        "stages": [], "queues": [], **over,
    }


def test_the_best_does_not_keep_queue_pins_it_was_not_measured_with(
    storage: Path,
) -> None:
    """The fake runner's winner ran ``engine`` at queue 1, derived, and said
    nothing of ``post``: neither of the row's pins is what was measured."""
    _library(storage, Alpha={"Volume 1": 20})
    rows = _gens(PRIMARY, QUEUED)
    worker = _worker(storage, rows, _script(storage))  # best: detect x3
    service = _service(storage, rows, worker)
    service.enqueue(rows[1].id, None, None)
    data = _wait_for(service, rows[1].id, "done")
    assert data["best"]["stage_workers"] == {"detect": 3}
    assert data["best"]["queue_capacity"] == {"engine": "auto", "post": "auto"}
    assert data["best"]["same_as_spec"] is False


def test_the_best_keeps_the_queue_pins_it_was_measured_with(storage: Path) -> None:
    _library(storage, Alpha={"Volume 1": 20})
    rows = _gens(PRIMARY, QUEUED)
    trial = _trial_at({"detect": 1, "engine": 4, "post": 4})
    worker = _worker(storage, rows, _script(
        storage, bench={"trials": [trial], "best": {**UNMOVED, "stage_workers": {"detect": 3}}},
    ))
    service = _service(storage, rows, worker)
    service.enqueue(rows[1].id, None, None)
    data = _wait_for(service, rows[1].id, "done")
    assert data["best"]["queue_capacity"] == {"engine": 4, "post": 4}
    assert data["best"]["same_as_spec"] is False, "detect was widened"


def test_a_best_that_moved_nothing_on_a_row_with_queue_pins_is_the_spec(
    storage: Path,
) -> None:
    _library(storage, Alpha={"Volume 1": 20})
    rows = _gens(PRIMARY, QUEUED)
    trial = _trial_at({"detect": 1, "engine": 4, "post": 4})
    worker = _worker(storage, rows, _script(
        storage, bench={"trials": [trial], "best": dict(UNMOVED)},
    ))
    service = _service(storage, rows, worker)
    service.enqueue(rows[1].id, None, None)
    data = _wait_for(service, rows[1].id, "done")
    assert data["best"]["queue_capacity"] == {"engine": 4, "post": 4}
    assert data["best"]["same_as_spec"] is True


def test_derived_queues_that_differ_from_the_pins_are_not_the_spec(
    storage: Path,
) -> None:
    """Widths and placement as the row has them, but the winner ran engine
    queue 1: the row's ``engine: 4`` was never measured, so this is a change."""
    _library(storage, Alpha={"Volume 1": 20})
    rows = _gens(PRIMARY, QUEUED)
    trial = _trial_at({"detect": 1, "engine": 1, "post": 4})
    worker = _worker(storage, rows, _script(
        storage, bench={"trials": [trial], "best": dict(UNMOVED)},
    ))
    service = _service(storage, rows, worker)
    service.enqueue(rows[1].id, None, None)
    data = _wait_for(service, rows[1].id, "done")
    assert data["best"]["queue_capacity"] == {"engine": "auto", "post": 4}
    assert data["best"]["same_as_spec"] is False
