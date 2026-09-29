"""Each machine's own history of a row: how many volumes it read, and its pages.

A processor's lifetime counts live in its profile (``runs.volumes`` /
``runs.pages``); THIS server's are kept the same way in ``@local.json``,
whichever road a volume took here (a session, or one volume per command
line), and the admin API sends them (``local_runs`` beside
``processor_runs``) so a card showing one machine can say what THAT machine
contributed.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from mokuro_bunko.admin.api import AdminAPI
from mokuro_bunko.config import Config
from mokuro_bunko.ocr.remote.profiles import LOCAL_PROFILE, ProcessorProfiles
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from tests.unit.test_ocr_sessions import (
    HAYAI,
    PRIMARY,
    _gens,
    _library,
    _script,
    _worker,
    storage,  # noqa: F401 - fixture
)
from tests.unit.test_remote_pools import (  # noqa: F401 - fixtures
    _no_published_catalog,
    _request,
    admin,
)


def _local_runs(where: Path, row_id: str) -> dict[str, Any]:
    raw = ProcessorProfiles(where).load(LOCAL_PROFILE)
    return dict((raw.get("rows") or {}).get(row_id, {}).get("runs") or {})


def test_this_server_s_session_volumes_are_counted_in_its_own_profile(
    storage: Path,  # noqa: F811
) -> None:
    rows = _gens(PRIMARY, HAYAI)
    _library(storage, Alpha=["Volume 1", "Volume 2"])
    worker = _worker(storage, rows, script=_script(storage, pages=3))
    worker._scan_ocr_once()
    runs = _local_runs(storage, rows[1].id)
    assert runs["volumes"] == 2
    assert runs["pages"] == 6
    assert runs["pages_per_second"] > 0
    # The recipe it was read with, like a processor's entry.
    raw = ProcessorProfiles(storage).load(LOCAL_PROFILE)["rows"][rows[1].id]
    assert raw["recipe"] == list(rows[1].output_affecting())


def test_a_one_volume_command_line_run_is_counted_too(tmp_path: Path) -> None:
    from mokuro_bunko.ocr.processor import OCRProcessor

    rows = _gens(PRIMARY, HAYAI)
    recorded: list[tuple[str, int, float]] = []
    proc = OCRProcessor(storage_path=tmp_path, generations=rows)
    proc.run_recorder = lambda row, pages, seconds: recorded.append((row.id, pages, seconds))
    out = tmp_path / "out"
    (out / "_ocr" / "x").mkdir(parents=True)
    for n in range(4):
        (out / "_ocr" / "x" / f"{n}.json").write_text("{}", encoding="utf-8")
    import time

    proc._record_run_rate(rows[1], out, time.time() - 2.0)
    assert [(gen, pages) for gen, pages, _seconds in recorded] == [(rows[1].id, 3)]


def test_the_admin_api_sends_this_server_s_counts(
    admin: tuple[AdminAPI, Config, ProcessorRegistry],  # noqa: F811
) -> None:
    app, config, _registry = admin
    row = config.ocr.generations[1]
    ProcessorProfiles(config.storage.base_path).record_run(
        LOCAL_PROFILE, row.id, pages=120, seconds=60.0, congestion=None,
        recipe=row.output_affecting(),
    )
    status, body = _request(app, "GET", "/_admin/api/ocr/generations")
    assert status == 200
    hayai = body["generations"][1]
    assert hayai["local_runs"]["volumes"] == 1
    assert hayai["local_runs"]["pages"] == 120
    # A row this server never read says so with nothing, not zeros.
    assert body["generations"][0].get("local_runs") is None
