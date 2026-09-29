"""A generation card's Device select lists the CHOSEN machine's devices.

The owner's report: with Machine set to ``tower (NVIDIA GeForce RTX 4090)``
the ``mokuro`` stage offered this LIBRARY's Ryzen and Radeon -- the merged
catalog's labels, whose first entries are this server's. Each machine's
select is built from that machine's own catalog (this server's probe, a
processor's registration, an offline one's remembered ``processors/<name>.json``),
anything nobody reported reads as plain ``CPU`` / ``GPU <n>``, and a pools
save is held to the devices of the machine it is for.
"""

from __future__ import annotations

from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.admin.api import AdminAPI
from mokuro_bunko.config import AdminConfig, Config
from mokuro_bunko.database import Database
from mokuro_bunko.ocr.devices import (
    DeviceCatalog,
    GpuDevice,
    catalog_from_processor,
    set_cached_catalog,
)
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.remote.profiles import ProcessorProfiles
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry

from .test_remote_pools import ROWS, _Control, _nothing, _request

# This library: the box in the screenshot.
LIBRARY = DeviceCatalog(
    gpus=(GpuDevice(index=0, name="AMD Radeon RX 9070 XT", memory_bytes=17_000_000_000),),
    cpu_label="AMD Ryzen 9 7950X 16-Core Processor (16 cores)",
    vendor="rocm",
    probed=True,
)
# tower, as a processor registers it (`processor.cli._catalog`: the probe's
# `DeviceCatalog.entries()`, rendered on tower).
TOWER_HOST = {
    "cpu": "AMD Ryzen Threadripper 9960X 24-Cores (48 cores)",
    "gpu": "NVIDIA GeForce RTX 4090",
    "backend": "cuda",
}
TOWER_CATALOG: dict[str, Any] = {
    "engines": ["mokuro", "hayai-nova"],
    "detectors": ["ctd"],
    "devices": [
        {"id": "auto", "label": "Auto — GPU 0 when available"},
        {"id": "cpu", "label": "AMD Ryzen Threadripper 9960X 24-Cores (48 cores)"},
        {"id": "gpu:0", "label": "GPU 0 — NVIDIA GeForce RTX 4090 (25 GB)"},
    ],
    "gpus": [{"index": 0, "formats": {"bf16": True, "fp16": True}}],
    "serves_mokuro": True,
}
LIBRARY_NAMES = ("Ryzen 9 7950X", "Radeon")


@pytest.fixture
def admin(tmp_path: Path) -> Iterator[tuple[AdminAPI, Config, ProcessorRegistry]]:
    storage = tmp_path / "storage"
    for name in ("library", "inbox", "users"):
        (storage / name).mkdir(parents=True)
    config = Config()
    config.storage.base_path = storage
    config.ocr.generations = parse_generation_list([dict(r) for r in ROWS])
    registry = ProcessorRegistry(local_name="this server")
    app = AdminAPI(
        _nothing, Database(storage / "mokuro.db"), AdminConfig(enabled=True, path="/_admin"),
        full_config=config, config_path=tmp_path / "config.yaml",
        ocr_control=_Control(registry),  # type: ignore[arg-type]
    )
    set_cached_catalog(LIBRARY)
    yield app, config, registry
    set_cached_catalog(None)


def _tower(
    registry: ProcessorRegistry,
    *,
    catalog: dict[str, Any] | None = None,
    host: dict[str, Any] | None = None,
    connected: bool = True,
) -> Any:
    entry = registry.register(
        username="tower", name="tower", host=dict(TOWER_HOST if host is None else host),
        catalog=dict(TOWER_CATALOG if catalog is None else catalog), max_sessions=1,
    )
    entry.stream_open = connected
    return entry


def _mokuro_stage(stages: list[dict[str, Any]]) -> dict[str, Any]:
    return next(s for s in stages if s["key"] == "mokuro")


def _labels(stage: dict[str, Any]) -> dict[str, str]:
    return {o["id"]: o["label"] for o in stage["device_options"]}


def _derive(app: AdminAPI, config: Config, machine: str | None) -> tuple[int, dict[str, Any]]:
    body: dict[str, Any] = {"spec": config.ocr.generations[0].to_dict()}
    if machine is not None:
        body["processor"] = machine
    return _request(app, "POST", "/_admin/api/ocr/generations/derive", body)


def _no_library_names(labels: dict[str, str]) -> None:
    for label in labels.values():
        for name in LIBRARY_NAMES:
            assert name not in label, f"{label!r} names this library's hardware"


class TestEachMachineListsItsOwnDevices:
    def test_the_payload_carries_every_processor_s_own_device_options(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, _config, registry = admin
        _tower(registry)
        status, body = _request(app, "GET", "/_admin/api/ocr/generations")
        assert status == 200
        mokuro = body["generations"][0]
        local = _labels(_mokuro_stage(mokuro["stages"]))
        assert local["cpu"] == "AMD Ryzen 9 7950X 16-Core Processor (16 cores)"
        assert local["gpu:0"] == "GPU 0 — AMD Radeon RX 9070 XT (17 GB)"
        tower_stage = _mokuro_stage(mokuro["processor_stages"]["tower"])
        tower = _labels(tower_stage)
        assert tower_stage["devices_allowed"] == ["auto", "cpu", "gpu:0"]
        assert tower == {
            "auto": "Auto → GPU 0",
            "cpu": "AMD Ryzen Threadripper 9960X 24-Cores (48 cores)",
            "gpu:0": "GPU 0 — NVIDIA GeForce RTX 4090 (25 GB)",
        }

    def test_derive_for_a_processor_labels_that_machine_s_devices(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, registry = admin
        _tower(registry)
        status, body = _derive(app, config, "tower")
        assert status == 200, body
        labels = _labels(_mokuro_stage(body["stages"]))
        assert labels["cpu"].startswith("AMD Ryzen Threadripper 9960X")
        assert labels["gpu:0"] == "GPU 0 — NVIDIA GeForce RTX 4090 (25 GB)"
        _no_library_names(labels)

    def test_auto_says_what_it_resolves_to_even_with_another_choice_made(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        """The stage is on the CPU now; ``auto`` would still put it on card 0."""
        app, config, registry = admin
        _tower(registry)
        spec = config.ocr.generations[0].to_dict()
        spec["pools"] = {"stage_device": {"mokuro": "cpu"}}
        status, body = _request(app, "POST", "/_admin/api/ocr/generations/derive",
                                {"spec": spec, "processor": "tower"})
        assert status == 200, body
        stage = _mokuro_stage(body["stages"])
        assert stage["device"] == "cpu"
        assert _labels(stage)["auto"] == "Auto → GPU 0"


class TestTheSaveIsHeldToThatMachine:
    def test_a_card_only_the_library_has_is_refused_for_the_processor(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, registry = admin
        set_cached_catalog(DeviceCatalog(
            gpus=(GpuDevice(0, "RX 9070 XT"), GpuDevice(1, "RX 7900 XTX")), probed=True,
        ))
        _tower(registry)
        row = config.ocr.generations[0]
        status, body = _request(
            app, "PUT", f"/_admin/api/ocr/generations/{row.id}/pools",
            {"processor": "tower", "pools": {"stage_device": {"mokuro": "gpu:1"}}},
        )
        assert status == 400, body
        assert "gpu:1" in body["error"] and "tower" in body["error"]
        assert "this server" not in body["error"]
        assert ProcessorProfiles(config.storage.base_path).row("tower", row.id) is None

    def test_a_device_the_processor_has_is_saved(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, registry = admin
        _tower(registry)
        row = config.ocr.generations[0]
        status, body = _request(
            app, "PUT", f"/_admin/api/ocr/generations/{row.id}/pools",
            {"processor": "tower", "pools": {"stage_device": {"mokuro": "gpu:0"}}},
        )
        assert status == 200, body
        assert body["pools"]["stage_device"] == {"mokuro": "gpu:0"}


class TestOfflineMachinesUseTheirRememberedCatalog:
    def test_a_disconnected_processor_keeps_its_devices(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, registry = admin
        _tower(registry, connected=False)
        status, body = _derive(app, config, "tower")
        assert status == 200, body
        labels = _labels(_mokuro_stage(body["stages"]))
        assert labels["gpu:0"] == "GPU 0 — NVIDIA GeForce RTX 4090 (25 GB)"
        status, payload = _request(app, "GET", "/_admin/api/ocr/generations")
        assert status == 200
        stages = payload["generations"][0]["processor_stages"]["tower"]
        assert _labels(_mokuro_stage(stages))["gpu:0"].endswith("RTX 4090 (25 GB)")

    def test_a_machine_known_only_by_its_profile_is_derived_and_saved_by_it(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, _registry = admin
        set_cached_catalog(DeviceCatalog(
            gpus=(GpuDevice(0, "RX 9070 XT"), GpuDevice(1, "RX 7900 XTX")), probed=True,
        ))
        ProcessorProfiles(config.storage.base_path).set_identity(
            "tower", host=TOWER_HOST, catalog=TOWER_CATALOG
        )
        status, body = _derive(app, config, "tower")
        assert status == 200, body
        labels = _labels(_mokuro_stage(body["stages"]))
        assert set(labels) == {"auto", "cpu", "gpu:0"}
        assert labels["cpu"].startswith("AMD Ryzen Threadripper 9960X")
        row = config.ocr.generations[0]
        refused, error = _request(
            app, "PUT", f"/_admin/api/ocr/generations/{row.id}/pools",
            {"processor": "tower", "pools": {"stage_device": {"mokuro": "gpu:1"}}},
        )
        assert refused == 400 and "tower" in error["error"]
        saved, _body = _request(
            app, "PUT", f"/_admin/api/ocr/generations/{row.id}/pools",
            {"processor": "tower", "pools": {"stage_device": {"mokuro": "gpu:0"}}},
        )
        assert saved == 200

    def test_a_processor_still_installing_falls_back_to_what_it_reported_before(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        """Re-registering mid-install reports no devices; its last profile did."""
        app, config, registry = admin
        ProcessorProfiles(config.storage.base_path).set_identity(
            "tower", host=TOWER_HOST, catalog=TOWER_CATALOG
        )
        _tower(registry, catalog={"engines": [], "detectors": []})
        status, body = _derive(app, config, "tower")
        assert status == 200, body
        assert _labels(_mokuro_stage(body["stages"]))["gpu:0"].endswith("RTX 4090 (25 GB)")

    def test_a_machine_nobody_knows_is_refused(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, _registry = admin
        status, body = _derive(app, config, "nobody")
        assert status == 400 and "nobody" in body["error"]
        row = config.ocr.generations[0]
        status, body = _request(
            app, "PUT", f"/_admin/api/ocr/generations/{row.id}/pools",
            {"processor": "nobody", "pools": {"stage_device": {"mokuro": "cpu"}}},
        )
        assert status == 400 and "nobody" in body["error"]


class TestUnknownDeviceInfoIsGeneric:
    def test_devices_without_labels_read_as_cpu_and_gpu_n(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, registry = admin
        _tower(
            registry,
            host={},
            catalog={**TOWER_CATALOG, "devices": [
                {"id": "auto"}, {"id": "cpu"}, {"id": "gpu:0"}, {"id": "gpu:1", "label": ""},
            ]},
        )
        status, body = _derive(app, config, "tower")
        assert status == 200, body
        labels = _labels(_mokuro_stage(body["stages"]))
        assert labels == {"auto": "Auto → GPU 0", "cpu": "CPU", "gpu:0": "GPU 0",
                          "gpu:1": "GPU 1"}

    def test_the_host_line_names_what_the_catalog_left_blank(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        """An older processor's rows without labels: its OWN host line is known."""
        app, config, registry = admin
        _tower(registry, catalog={**TOWER_CATALOG, "devices": [
            {"id": "auto"}, {"id": "cpu"}, {"id": "gpu:0"},
        ]})
        status, body = _derive(app, config, "tower")
        assert status == 200, body
        labels = _labels(_mokuro_stage(body["stages"]))
        assert labels["cpu"] == "AMD Ryzen Threadripper 9960X 24-Cores (48 cores)"
        assert labels["gpu:0"] == "GPU 0 — NVIDIA GeForce RTX 4090"

    def test_a_processor_that_reported_no_devices_offers_auto_and_cpu_only(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, registry = admin
        _tower(registry, host={}, catalog={"engines": ["mokuro"], "detectors": []})
        status, body = _derive(app, config, "tower")
        assert status == 200, body
        labels = _labels(_mokuro_stage(body["stages"]))
        assert set(labels) == {"auto", "cpu"}
        assert labels["cpu"] == "CPU"
        _no_library_names(labels)


class TestTheCatalogLabels:
    def test_a_card_nobody_named_is_gpu_n(self) -> None:
        assert GpuDevice(index=1, name="").label == "GPU 1"

    def test_label_for_is_generic_where_the_catalog_is_silent(self) -> None:
        catalog = catalog_from_processor({"devices": [{"id": "cpu"}, {"id": "gpu:0"}]})
        assert catalog.label_for("cpu") == "CPU"
        assert catalog.label_for("gpu:0") == "GPU 0"
        assert catalog.label_for("gpu:3") == "GPU 3"

    def test_a_refusal_names_the_machine(self) -> None:
        catalog = catalog_from_processor(TOWER_CATALOG)
        assert "this server" in catalog.refusal("gpu:1")
        named = catalog.for_machine("tower", TOWER_HOST)
        assert "tower" in named.refusal("gpu:1")
        assert "this server" not in named.refusal("gpu:1")
