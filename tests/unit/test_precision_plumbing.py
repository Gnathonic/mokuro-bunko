"""A row's precision MODE on the wire: the admin API, the pools and the bench.

The mode itself (``precision`` on a row) and how each machine resolves it are
``test_precision_modes.py``'s. Here: what the admin panel is sent and may
save, that a machine's pools never carry a precision any more, and that a
benchmark's ``best`` never proposes one.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.admin.api import AdminAPI
from mokuro_bunko.config import Config
from mokuro_bunko.ocr.devices import DeviceCatalog, GpuDevice, set_cached_catalog
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.remote.profiles import ProcessorProfiles
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
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
from tests.unit.test_remote_bench import _registry, _remote_run
from tests.unit.test_remote_pools import (  # noqa: F401 - fixtures
    _entry,
    _no_published_catalog,
    _request,
    admin,
)

MOKURO = {"name": "mokuro", "engine": "mokuro", "primary": True}
MODES = ["auto-accuracy", "auto-balanced", "auto-speed", "fp32", "bf16", "fp16"]


def _rows(*extra: dict[str, Any]) -> Any:
    return parse_generation_list([dict(MOKURO), *[dict(row) for row in extra]])


class TestTheAdminApi:
    def test_the_catalog_names_the_modes_and_each_engine_s(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]  # noqa: F811
    ) -> None:
        app, _config, _registry = admin
        status, body = _request(app, "GET", "/_admin/api/ocr/generations")
        assert status == 200
        catalog = body["catalog"]
        assert "precisions" not in catalog
        assert [m["id"] for m in catalog["precision_modes"]] == MODES
        assert catalog["precision_modes"][0]["label"] == "Auto: accuracy"
        assert catalog["precision_default"] == "auto-accuracy"
        offered = {e["id"]: e["precision_modes"] for e in catalog["engines"]}
        assert offered["hayai-nova"] == MODES and offered["paddle-manga"] == MODES
        assert "bf16" not in offered["mokuro"] and "fp16" in offered["mokuro"]
        assert offered["ppocr-manga"] == []

    def test_each_row_says_its_mode_and_what_every_mode_does_on_every_machine(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]  # noqa: F811
    ) -> None:
        app, _config, registry = admin
        _entry(registry, "tower")  # a processor older than the card probe
        set_cached_catalog(DeviceCatalog(
            gpus=(GpuDevice(0, "RX 9070 XT", formats=frozenset({"bf16", "fp16"})),),
            probed=True,
        ))
        status, body = _request(app, "GET", "/_admin/api/ocr/generations")
        assert status == 200
        hayai = body["generations"][1]
        assert [g["precision_applies"] for g in body["generations"]] == [True, True]
        assert hayai["precision"] == "auto-accuracy" and hayai["precision_hold"] is None
        on = hayai["precision_on"]
        assert set(on) == {"local", "tower"}
        assert set(on["local"]) == set(MODES)
        assert on["local"]["auto-accuracy"] == {
            "precision": "bf16", "eligible": True, "why": "auto-accuracy",
        }
        assert on["local"]["auto-speed"]["why"].startswith("not benchmarked yet")
        # tower reported nothing about its cards: fp32-only for a forced
        # mode, its own runner's call for an auto one.
        assert on["tower"]["bf16"]["eligible"] is False
        assert on["tower"]["fp32"] == {"precision": "fp32", "eligible": True, "why": "fp32"}
        assert on["tower"]["auto-accuracy"]["eligible"] is True
        assert on["tower"]["auto-accuracy"]["precision"] is None

    def test_a_mode_is_saved_on_the_row(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]  # noqa: F811
    ) -> None:
        app, config, _registry = admin
        rows = [row.to_dict() for row in config.ocr.generations]
        rows[1]["precision"] = "auto-speed"
        status, body = _request(app, "PUT", "/_admin/api/ocr/generations", {"generations": rows})
        assert status == 200, body
        assert config.ocr.generations[1].precision == "auto-speed"
        assert config.ocr.generations[1].pools.is_empty(), "a mode is not a pools table"

    def test_a_bad_mode_is_refused_naming_the_field(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]  # noqa: F811
    ) -> None:
        app, config, _registry = admin
        rows = [row.to_dict() for row in config.ocr.generations]
        rows[1]["precision"] = "int8"
        status, body = _request(app, "PUT", "/_admin/api/ocr/generations", {"generations": rows})
        assert status == 400 and body["field"] == "precision"

    def test_a_processor_s_pools_never_keep_a_precision(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]  # noqa: F811
    ) -> None:
        app, config, registry = admin
        _entry(registry, "tower")
        row = config.ocr.generations[1]
        status, body = _request(
            app, "PUT", f"/_admin/api/ocr/generations/{row.id}/pools",
            {"processor": "tower", "pools": {"stage_workers": {"detect": 2}, "precision": "fp16"}},
        )
        assert status == 200, body
        assert "precision" not in body["pools"]
        raw = ProcessorProfiles(config.storage.base_path).load("tower")
        assert "precision" not in raw["rows"][row.id]["pools"]


def _best(**extra: Any) -> dict[str, Any]:
    return {"best": {"trial": 1, "stage_workers": {}, "queue_capacity": {}, "stage_device": {},
                     "pages_per_second": 2.0, "seconds_per_page": 0.5, "speedup": 1.0, **extra}}


class TestApplyingABenchmark:
    def test_a_precision_in_best_is_never_applied(self, storage: Path) -> None:  # noqa: F811
        """A runner older than the policy still reports the winner of its
        measured phase; the result proposes nothing about the precision."""
        _library(storage, Alpha={"Volume 1": 20})
        rows = _rows(HAYAI)
        worker = _bench_worker(
            storage, rows, _script(storage, bench=_best(precision="fp32", precision_auto="fp16"))
        )
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, None)
        data = _wait_for(service, rows[1].id, "done")
        assert "precision" not in data["best"] and "precision_auto" not in data["best"]
        assert data["precision"] == "fp32", "recorded as what it ran at"
        assert data["best"]["same_as_spec"] is True

    def test_a_forced_row_this_server_cannot_run_is_not_benchmarked_here(
        self, storage: Path  # noqa: F811
    ) -> None:
        from mokuro_bunko.ocr.bench import BenchError

        _library(storage, Alpha={"Volume 1": 20})
        rows = _rows({**HAYAI, "precision": "bf16"})
        set_cached_catalog(DeviceCatalog(
            gpus=(GpuDevice(0, "card", formats=frozenset({"fp16"})),), probed=True,
        ))
        worker = _bench_worker(storage, rows, _script(storage, bench=_best()))
        service = _service(storage, rows, worker)
        with pytest.raises(BenchError, match="cannot run bf16"):
            service.enqueue(rows[1].id, None, None)

    def test_a_forced_row_keeps_its_mode(self, storage: Path) -> None:  # noqa: F811
        _library(storage, Alpha={"Volume 1": 20})
        rows = _rows({**HAYAI, "precision": "bf16"})
        set_cached_catalog(DeviceCatalog(
            gpus=(GpuDevice(0, "card", formats=frozenset({"bf16", "fp16"})),), probed=True,
        ))
        worker = _bench_worker(storage, rows, _script(storage, bench=_best()))
        service = _service(storage, rows, worker)
        service.enqueue(rows[1].id, None, None)
        data = _wait_for(service, rows[1].id, "done")
        assert "precision" not in data["best"]
        assert data["best"]["same_as_spec"] is True
        assert rows[1].precision == "bf16"

    @pytest.mark.parametrize("precision", ["fp32", "auto"])
    def test_an_autobench_stores_no_precision_on_that_processor(
        self, storage: Path, precision: str  # noqa: F811
    ) -> None:
        registry, entry = _registry()
        service = remote_bench.TestChoosingAProcessor._service(storage, registry)
        run = _remote_run(service, entry, autobench=True)
        best = dict(run.data["best"])
        best.update(precision=precision, same_as_spec=False)
        run.update(best=best)
        service._finish(run, "done")
        row = ProcessorProfiles(storage).row("tower", "g-2")
        assert row is not None and "precision" not in row.pools
