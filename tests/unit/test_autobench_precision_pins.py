"""A precision is never a POOL: the row's mode is the one setting.

A finished benchmark's ``best`` never proposes a precision, and an automatic
one stores none in a machine's pools; what it ran at (and, for a
balanced/speed mode, its pick and trials) is recorded beside ``best``. A
precision still stored in a machine's pools -- an earlier automatic
benchmark's, or one a person saved for that machine before the modes -- is
ignored wherever it is read, said once per (machine, row) in the log.
"""

from __future__ import annotations

import json
import logging
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr.remote import profiles as profiles_module
from mokuro_bunko.ocr.remote.profiles import (
    LOCAL_PROFILE,
    POOLS_AUTOBENCH,
    ProcessorProfiles,
    profile_filename,
    profiles_dir,
)
from tests.unit import test_remote_bench as remote_bench
from tests.unit.test_ocr_bench import (
    HAYAI,
    _library,
    _script,
    _service,
    _wait_for,
    storage,  # noqa: F401 - fixture
)
from tests.unit.test_ocr_bench import _worker as _bench_worker
from tests.unit.test_precision_plumbing import _best, _rows
from tests.unit.test_remote_bench import _registry, _remote_run

# The recipe the profile rows below are measured with: hayai-nova + ctd.
RECIPE = ["hayai-nova", "ctd", 512]


@pytest.fixture(autouse=True)
def _fresh_log_memory() -> None:
    profiles_module._IGNORED_LOGGED.clear()


def _raw(where: Path, name: str) -> dict[str, Any]:
    path = profiles_dir(where) / profile_filename(name)
    return json.loads(path.read_text(encoding="utf-8"))


def _write_raw(where: Path, name: str, rows: dict[str, Any]) -> None:
    """A profile file as an earlier version of the server left it."""
    directory = profiles_dir(where)
    directory.mkdir(parents=True, exist_ok=True)
    (directory / profile_filename(name)).write_text(
        json.dumps({"name": name, "rows": rows}), encoding="utf-8"
    )


def _bench_row(storage: Path, row: dict[str, Any], bench: dict[str, Any]) -> dict[str, Any]:  # noqa: F811
    """One finished local benchmark of ``row`` against a scripted runner."""
    _library(storage, Alpha={"Volume 1": 20})
    rows = _rows(row)
    worker = _bench_worker(storage, rows, _script(storage, bench=bench))
    service = _service(storage, rows, worker)
    service.enqueue(rows[1].id, None, None)
    return _wait_for(service, rows[1].id, "done")


# --- what a finished benchmark applies ---------------------------------------------


class TestAFinishedBenchmark:
    def test_its_best_carries_no_precision_and_it_records_what_it_ran_at(
        self, storage: Path  # noqa: F811
    ) -> None:
        data = _bench_row(storage, HAYAI, {**_best(), "precision": "bf16"})
        assert "precision" not in data["best"]
        assert data["precision"] == "bf16"
        assert data["best"]["same_as_spec"] is True, "nothing else moved"

    def test_a_precision_an_older_runner_chose_is_never_applied(
        self, storage: Path  # noqa: F811
    ) -> None:
        """A processor not yet updated still runs the old measured phase and
        reports its winner in ``best``: recorded as what it ran at, applied
        nowhere."""
        data = _bench_row(
            storage, HAYAI,
            _best(precision="fp16", precision_auto="fp32", card_family="gfx1201"),
        )
        for key in ("precision", "precision_auto", "card_family", "precision_ran"):
            assert key not in data["best"], key
        assert data["precision"] == "fp16"
        assert data["best"]["same_as_spec"] is True

    def test_a_forced_row_is_measured_as_forced_and_nothing_is_proposed(
        self, storage: Path  # noqa: F811
    ) -> None:
        from mokuro_bunko.ocr.devices import DeviceCatalog, GpuDevice, set_cached_catalog

        set_cached_catalog(DeviceCatalog(
            gpus=(GpuDevice(0, "card", formats=frozenset({"fp16"})),), probed=True,
        ))
        try:
            data = _bench_row(
                storage, {**HAYAI, "precision": "fp16"}, {**_best(), "precision": "fp16"}
            )
        finally:
            set_cached_catalog(None)
        assert "precision" not in data["best"] and data["precision"] == "fp16"
        assert data["best"]["same_as_spec"] is True

    def test_this_server_s_saved_result_records_it(self, storage: Path) -> None:  # noqa: F811
        from mokuro_bunko.ocr.bench import bench_path

        data = _bench_row(storage, HAYAI, {**_best(), "precision": "bf16"})
        saved = json.loads(bench_path(storage).read_text(encoding="utf-8"))
        assert saved[data["key"]]["precision"] == "bf16"

    def test_a_row_with_no_torch_recognizer_records_none(
        self, storage: Path  # noqa: F811
    ) -> None:
        data = _bench_row(
            storage, {"name": "pp", "engine": "ppocr-manga"}, {**_best(), "precision": "fp32"}
        )
        assert data.get("precision") is None and "precision" not in data["best"]


class TestAnAutobench:
    def _autobench(self, where: Path, **best: Any) -> None:
        registry, entry = _registry()
        service = remote_bench.TestChoosingAProcessor._service(where, registry)
        run = _remote_run(service, entry, autobench=True)
        merged = dict(run.data["best"])
        merged.update(same_as_spec=False, **best)
        run.update(best=merged, precision="bf16")
        service._finish(run, "done")

    @pytest.mark.parametrize("precision", ["bf16", "fp16", "fp32"])
    def test_stores_no_precision_whatever_best_says(
        self, storage: Path, precision: str  # noqa: F811
    ) -> None:
        self._autobench(storage, precision=precision)
        raw = _raw(storage, "tower")["rows"]["g-2"]
        assert "precision" not in raw["pools"]
        assert raw["pools"]["stage_workers"] == {"detect": 3}, "the widths it found stay"
        assert POOLS_AUTOBENCH in raw

    def test_its_bench_summary_records_the_precision_it_ran_at(
        self, storage: Path  # noqa: F811
    ) -> None:
        self._autobench(storage)
        row = ProcessorProfiles(storage).row("tower", "g-2")
        assert row is not None and row.bench is not None
        assert row.bench["precision"] == "bf16"
        assert "precision_trials" not in row.bench


# --- what an earlier autobench stored, read back ---------------------------------------


class TestAPrecisionStoredInAMachinesPools:
    @pytest.mark.parametrize("precision", ["bf16", "fp16", "fp32", "auto"])
    @pytest.mark.parametrize("autobench", [True, False], ids=["autobench", "person"])
    def test_is_ignored_on_every_read_with_one_log_line(
        self, storage: Path, caplog: pytest.LogCaptureFixture,  # noqa: F811
        precision: str, autobench: bool,
    ) -> None:
        _write_raw(storage, "tower", {"g-2": {
            "recipe": RECIPE,
            "pools": {"stage_workers": {"detect": 3}, "precision": precision},
            **({POOLS_AUTOBENCH: {}} if autobench else {}),
        }})
        store = ProcessorProfiles(storage)
        with caplog.at_level(logging.INFO, logger="mokuro_bunko.ocr.remote.profiles"):
            for _ in range(3):
                row = store.row("tower", "g-2", recipe=RECIPE)
                assert row is not None and "precision" not in row.pools
                assert row.pools["stage_workers"] == {"detect": 3}, "the rest of it stays"
        said = [r.getMessage() for r in caplog.records if "Ignoring the precision" in r.getMessage()]
        assert len(said) == 1, "said once, not on every read"
        assert "tower" in said[0] and precision in said[0] and "every machine" in said[0]

    def test_a_precision_alone_leaves_no_pools(self, storage: Path) -> None:  # noqa: F811
        _write_raw(storage, "tower", {"g-2": {"recipe": RECIPE, "pools": {"precision": "bf16"}}})
        row = ProcessorProfiles(storage).row("tower", "g-2", recipe=RECIPE)
        assert row is not None and row.pools == {}, "the row's own table"

    def test_one_is_never_written(self, storage: Path) -> None:  # noqa: F811
        store = ProcessorProfiles(storage)
        store.set_pools("tower", "g-2", {"stage_workers": {"detect": 2}, "precision": "fp16"},
                        recipe=RECIPE)
        assert "precision" not in _raw(storage, "tower")["rows"]["g-2"]["pools"]

    def test_this_server_s_profile_the_same(self, storage: Path) -> None:  # noqa: F811
        _write_raw(storage, LOCAL_PROFILE, {"g-2": {"recipe": RECIPE, "pools": {
            "stage_workers": {"detect": 2}, "precision": "fp32"}}})
        row = ProcessorProfiles(storage).row(LOCAL_PROFILE, "g-2", recipe=RECIPE)
        assert row is not None and "precision" not in row.pools
        assert row.pools["stage_workers"] == {"detect": 2}

    def test_the_row_s_mode_reaches_the_runner(self, tmp_path: Path) -> None:
        from tests.unit.test_local_autobench import _flag, _worker

        rows = _rows({**HAYAI, "precision": "fp16"})
        worker = _worker(tmp_path, rows)
        command = worker._slots[0].processor.session_command(rows[1], tmp_path / "s.log")
        assert _flag(command, "--precision") == "fp16"


class TestWhatRunsFromIt:
    def test_this_server_runs_the_row_s_mode_whatever_its_profile_stored(
        self, tmp_path: Path
    ) -> None:
        from tests.unit.test_local_autobench import _flag, _worker

        worker = _worker(tmp_path)
        row = worker.generations[1]
        _write_raw(tmp_path, LOCAL_PROFILE, {row.id: {
            "recipe": list(row.output_affecting()),
            "pools": {"stage_workers": {"detect": 3}, "precision": "fp32"},
        }})
        command = worker._slots[0].processor.session_command(row, tmp_path / "s.log")
        assert _flag(command, "--stage-workers") == "detect=3"
        assert "--precision" not in command, "the default mode: the runner's own default"
