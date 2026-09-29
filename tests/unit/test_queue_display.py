"""The queue page's display levels, its redaction, and its cheap polling.

* `queue.shape`: what each level SENDS -- exact key sets, one entry per
  machine with the on-deck volume folded into a "next" line, and the
  exposure rule (raw errors, log paths, hardware labels and the backend reach
  an admin only);
* `QueueAPI`: the same through HTTP for an anonymous visitor and an admin,
  and the ETag/304 path that answers an unchanged poll without planning or
  serializing anything;
* `OCRWorker`: the queue-state version moving on every event kind the page
  shows, and the per-generation speed readout.
"""

from __future__ import annotations

import base64
import io
import json
import threading
import time
import zipfile
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, QueueConfig, set_by_dotted_key
from mokuro_bunko.database import Database
from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.eta import RateModel
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.processor import OcrFailure
from mokuro_bunko.ocr.remote.protocol import PROTOCOL_VERSION
from mokuro_bunko.ocr.watcher import OCRWorker
from mokuro_bunko.queue import api as queue_api_module
from mokuro_bunko.queue.api import QueueAPI
from mokuro_bunko.queue.shape import (
    REASON_ARCHIVE,
    REASON_DOWNLOAD,
    REASON_ENGINE,
    REASON_INTERRUPTED,
    REASON_RETRY,
    PublicNames,
    failure_reason,
    shape_status,
)

ERROR_TEXT = "engine runner exited with code 1: CUDA out of memory at /srv/models/x.onnx"
LOG_PATH = "/srv/bunko/logs/ocr/Series A_Volume 01.nova.log"
LABEL = "tower (RTX 4090)"


def _card(**extra: Any) -> dict[str, Any]:
    card = {
        "series": "Series A", "volume": "Volume 01", "generation": "nova",
        "generation_id": "g-2", "engine": "hayai-nova", "detector": "ctd",
        "percent": 40, "done_pages": 40, "total_pages": 100, "status": "running",
        "eta_seconds": 60, "eta_at": "2026-09-22T05:00:00Z", "slot": 0,
        "started_at": 100.0, "machine": "local", "processor": None,
        "rate_pages_per_second": 1.0, "rate_source": "session", "latency_seconds": 2.0,
        "startup_seconds": None, "startup_rough": False,
        "pipeline": {"verdict": "widen detect", "bottleneck": "detect",
                     "stages": [{"key": "detect", "device": "gpu:0"}]},
    }
    card.update(extra)
    return card


def _raw() -> dict[str, Any]:
    """Two machines working, one with a volume on deck behind its active one."""
    return {
        "current_jobs": [
            _card(),
            _card(volume="Volume 02", percent=0, done_pages=0, status="starting",
                  started_at=101.0, eta_at="2026-09-22T05:10:00Z"),
            _card(volume="Volume 07", slot=1, machine="tower", processor=LABEL,
                  started_at=99.0, generation="mokuro", generation_id="g-1"),
        ],
        "pending_ocr": [
            {"series": "Series B", "volume": "Volume 01", "generation": "nova",
             "generation_id": "g-2", "engine": "hayai-nova", "detector": "ctd",
             "pages": 120, "eta_at": "2026-09-22T06:00:00Z", "eta_seconds": 900,
             "rough": False, "reason": None, "rate_source": "session",
             "latency_seconds": 2.0},
        ],
        "queue_done_at": "2026-09-22T06:00:00Z",
        "pending_thumbnails": 3,
        "failed": [
            {"series": "Series C", "volume": "Volume 03", "generation": "nova",
             "engine": "hayai-nova", "detector": "ctd", "error": ERROR_TEXT,
             "attempts": 2, "last_attempt_at": 1790000000.0, "log_file": LOG_PATH},
        ],
        "backend": "rocm",
        "generations": [{"id": "g-1", "name": "mokuro", "engine": "mokuro", "detector": None},
                        {"id": "g-2", "name": "nova", "engine": "hayai-nova", "detector": "ctd"}],
        "skipped_missing_pages": [],
        "paused_for_benchmark": None,
        "processing_hold": None,
        "speed": [
            {"generation": "nova", "generation_id": "g-2",
             "machines": [{"machine": "tower", "pages_per_minute": 48.0, "volumes": 12,
                           "lanes": 0, "working": False},
                          {"machine": "local", "pages_per_minute": 21.0, "volumes": 4,
                           "lanes": 1, "working": True}],
             "combined_pages_per_minute": 21.0},
            {"generation": "mokuro", "generation_id": "g-1",
             "machines": [{"machine": "tower", "pages_per_minute": 30.0, "volumes": 3,
                           "lanes": 1, "working": True}],
             "combined_pages_per_minute": 30.0},
            {"generation": "paddle", "generation_id": "g-3",
             "machines": [{"machine": "tower", "pages_per_minute": 9.0, "volumes": 2,
                           "lanes": 0, "working": False}],
             "combined_pages_per_minute": None},
        ],
    }


COMMON = {"level", "queue_done_at", "pending_count", "processing_hold",
          "paused_for_benchmark", "machines"}
NORMAL_TOP = COMMON | {"pending", "pending_thumbnails", "failed", "failed_count",
                       "skipped_missing_pages", "generations"}
MINIMAL_JOB = {"series", "volume", "generation", "percent", "eta_at", "state"}
NORMAL_JOB = MINIMAL_JOB | {"status", "done_pages", "total_pages", "eta_seconds",
                            "startup_seconds", "host_busy"}
DETAILED_JOB = NORMAL_JOB | {"engine", "detector", "throughput_pages_per_minute",
                             "latency_seconds", "startup_rough", "pipeline"}
PENDING = {"series", "volume", "generation", "eta_at", "rough", "attempts", "reason",
           "returned"}
DETAILED_PENDING = PENDING | {"engine", "detector", "pages", "rate_source", "latency_seconds"}
FAILED = {"series", "volume", "generation", "attempts", "reason"}


class TestLevels:
    def test_minimal_sends_one_line_per_machine_and_nothing_else(self) -> None:
        for admin in (False, True):
            data = shape_status(_raw(), "minimal", admin=admin)
            # An admin is also told which rows no machine can run.
            assert set(data) == COMMON | {"pending"} | ({"held_rows"} if admin else set())
            assert set(data["pending"][0]) == {"series", "volume", "generation", "eta_at", "rough"}
            tower = "tower" if admin else "machine 1"
            assert [m["name"] for m in data["machines"]] == ["this server", tower]
            for machine in data["machines"]:
                assert set(machine) == {"name", "state", "slots", "jobs", "next"}
                for job in machine["jobs"]:
                    assert set(job) == MINIMAL_JOB
            # The on-deck volume is not a line of its own at any level: it is
            # the card's on-deck field, at minimal too.
            assert [j["volume"] for j in data["machines"][0]["jobs"]] == ["Volume 01"]
            assert data["machines"][0]["next"] == [
                {"series": "Series A", "volume": "Volume 02", "generation": "nova"}
            ]
            assert data["machines"][1]["next"] == []
            assert data["pending_count"] == 1

    def test_normal_is_a_card_per_machine_with_a_next_line(self) -> None:
        data = shape_status(_raw(), "normal", admin=False)
        assert set(data) == NORMAL_TOP
        local, tower = data["machines"]
        assert set(local) == {"name", "state", "slots", "jobs", "next"}
        assert [j["volume"] for j in local["jobs"]] == ["Volume 01"]
        assert local["next"] == [
            {"series": "Series A", "volume": "Volume 02", "generation": "nova"}
        ]
        assert tower["name"] == "machine 1" and tower["next"] == []
        for machine in data["machines"]:
            for job in machine["jobs"]:
                assert set(job) == NORMAL_JOB
        assert set(data["pending"][0]) == PENDING
        assert set(data["failed"][0]) == FAILED
        assert data["failed_count"] == 1
        assert data["generations"] == [{"id": "g-1", "name": "mokuro"},
                                       {"id": "g-2", "name": "nova"}]

    def test_detailed_adds_the_tuning_readouts(self) -> None:
        data = shape_status(_raw(), "detailed", admin=False)
        assert set(data) == NORMAL_TOP | {"speed"}
        job = data["machines"][0]["jobs"][0]
        assert set(job) == DETAILED_JOB
        assert job["pipeline"]["verdict"] == "widen detect"
        assert set(data["pending"][0]) == DETAILED_PENDING

    def test_an_unknown_level_is_normal(self) -> None:
        assert shape_status(_raw(), "verbose", admin=False)["level"] == "normal"

    @pytest.mark.parametrize("level", ["minimal", "normal"])
    @pytest.mark.parametrize("admin", [False, True])
    def test_minimal_and_normal_send_no_speed(self, level: str, admin: bool) -> None:
        """Owner: the Speed section is too much detail below `detailed`."""
        data = shape_status(_raw(), level, admin=admin)
        assert "speed" not in data
        assert "pages_per_minute" not in json.dumps(data)
        for machine in data["machines"]:
            for job in machine["jobs"]:
                assert "rate_pages_per_second" not in job

    @pytest.mark.parametrize("admin", [False, True])
    def test_detailed_sends_one_combined_line_per_layer_being_read(self, admin: bool) -> None:
        raw = _raw()
        # A second machine reading nova: the line is the two together.
        raw["speed"][0]["machines"][0].update(lanes=1, working=True)
        raw["speed"][0]["combined_pages_per_minute"] = 69.0
        data = shape_status(raw, "detailed", admin=admin)
        # paddle: nobody reading it now, so no line at all.
        assert data["speed"] == [
            {"generation": "nova", "pages_per_minute": 69.0, "machines": 2},
            {"generation": "mokuro", "pages_per_minute": 30.0, "machines": 1},
        ]
        # No per-machine breakdown in public, not even for an admin: that
        # is the admin panel's Processors card.
        assert "tower" not in json.dumps(data["speed"])
        assert "48.0" not in json.dumps(data["speed"])

    def test_a_detailed_card_shows_its_machines_real_throughput(self) -> None:
        data = shape_status(_raw(), "detailed", admin=True)
        by_name = {m["name"]: m for m in data["machines"]}
        assert by_name["this server"]["jobs"][0]["throughput_pages_per_minute"] == 21.0
        # tower is reading mokuro (g-1): ITS number on that layer, not nova's.
        assert by_name["tower"]["jobs"][0]["throughput_pages_per_minute"] == 30.0
        for machine in data["machines"]:
            for job in machine["jobs"]:
                assert "rate_pages_per_second" not in job

    def test_minimal_pending_is_capped_and_compact(self) -> None:
        raw = _raw()
        raw["pending_ocr"] = [dict(raw["pending_ocr"][0], volume=f"V{n}") for n in range(25)]
        data = shape_status(raw, "minimal", admin=True)
        assert len(data["pending"]) == 10
        assert [p["volume"] for p in data["pending"]] == [f"V{n}" for n in range(10)]
        for item in data["pending"]:
            assert set(item) == {"series", "volume", "generation", "eta_at", "rough"}
        assert data["pending_count"] == 25


class TestExposure:
    @pytest.mark.parametrize("level", ["minimal", "normal", "detailed"])
    def test_a_visitor_never_gets_errors_paths_labels_or_backend(self, level: str) -> None:
        text = json.dumps(shape_status(_raw(), level, admin=False))
        for secret in (ERROR_TEXT, "CUDA", "/srv", "RTX 4090", LOG_PATH, "rocm"):
            assert secret not in text, (level, secret)

    def test_a_visitor_gets_the_reason_category(self) -> None:
        (failed,) = shape_status(_raw(), "normal", admin=False)["failed"]
        assert failed["reason"] == REASON_ENGINE
        assert "error" not in failed and "log_file" not in failed

    @pytest.mark.parametrize("level", ["normal", "detailed"])
    def test_an_admin_gets_them(self, level: str) -> None:
        data = shape_status(_raw(), level, admin=True)
        (failed,) = data["failed"]
        assert failed["error"] == ERROR_TEXT
        assert failed["log_file"] == LOG_PATH
        assert data["machines"][1]["label"] == LABEL
        assert data["backend"] == "rocm"

    def test_a_returned_job_says_so_by_the_exposure_rule(self) -> None:
        raw = _raw()
        raw["pending_ocr"][0]["returned"] = {
            "count": 1, "class": "stalled", "machine": "tower", "at": 1790000000.0,
            "error": "no new byte for 120 s at byte 104,857,600 of 345,600,000",
        }
        names = PublicNames()
        names.assign("tower")
        for level in ("normal", "detailed"):
            visitor = shape_status(raw, level, admin=False, public_names=names)
            returned = visitor["pending"][0]["returned"]
            assert returned == {"machine": "machine 1", "reason": REASON_DOWNLOAD,
                                "at": 1790000000.0}
            assert "104,857,600" not in json.dumps(visitor)
            admin = shape_status(raw, level, admin=True)["pending"][0]["returned"]
            assert admin["machine"] == "tower" and admin["class"] == "stalled"
            assert admin["error"].startswith("no new byte")
        minimal = shape_status(raw, "minimal", admin=True)
        assert "returned" not in minimal["pending"][0]

    def test_a_machine_held_for_failing_downloads_shows_it(self) -> None:
        raw = _raw()
        raw["current_jobs"] = [job for job in raw["current_jobs"] if job["machine"] != "tower"]
        raw["connected_machines"] = [
            {"machine": "local", "slots": 1},
            {"machine": "tower", "slots": 1, "held": "downloads",
             "held_until": 1790000600.0, "held_error": "missing: the library has no ... (404)"},
        ]
        visitor = shape_status(raw, "normal", admin=False)
        tower = visitor["machines"][1]
        assert tower["state"] == "held"
        assert tower["held"] == {"reason": "downloads failing", "until": 1790000600.0}
        admin = shape_status(raw, "normal", admin=True)["machines"][1]
        assert admin["held"]["error"].startswith("missing")
        assert shape_status(raw, "minimal", admin=False)["machines"][1]["state"] == "held"

    def test_a_machine_being_benchmarked_says_what_it_is_configuring(self) -> None:
        """Not "Idle": a card that sat idle through its benchmark read as a
        machine the library had lost, or forgotten to schedule."""
        raw = _raw()
        raw["current_jobs"] = [job for job in raw["current_jobs"] if job["machine"] != "tower"]
        raw["connected_machines"] = [
            {"machine": "local", "slots": 1},
            {"machine": "tower", "slots": 1,
             "configuring": {"key": "g-2", "generation": "hayai-nova-ppocr", "auto": True}},
        ]
        for level in ("minimal", "normal", "detailed"):
            for admin in (False, True):
                tower = shape_status(raw, level, admin=admin)["machines"][1]
                assert tower["state"] == "configuring", (level, admin)
                assert tower["configuring"] == {"generation": "hayai-nova-ppocr", "auto": True}

    def test_a_draft_being_benchmarked_is_named_to_admins_only(self) -> None:
        raw = _raw()
        raw["current_jobs"] = [job for job in raw["current_jobs"] if job["machine"] != "tower"]
        raw["connected_machines"] = [
            {"machine": "local", "slots": 1},
            {"machine": "tower", "slots": 1,
             "configuring": {"key": "draft-abc", "generation": "draft-abc", "auto": False}},
        ]
        visitor = shape_status(raw, "normal", admin=False)["machines"][1]
        assert visitor["configuring"] == {"generation": None, "auto": False}
        admin = shape_status(raw, "normal", admin=True)["machines"][1]
        assert admin["configuring"] == {"generation": "draft-abc", "auto": False}

    def test_work_on_the_card_outranks_a_benchmark(self) -> None:
        raw = _raw()
        tower_jobs = [job for job in raw["current_jobs"] if job["machine"] == "tower"]
        assert tower_jobs, "the fixture has tower reading a volume"
        raw["connected_machines"] = [
            {"machine": "local", "slots": 1},
            {"machine": "tower", "slots": 1,
             "configuring": {"key": "g-2", "generation": "hayai-nova-ppocr", "auto": True}},
        ]
        tower = shape_status(raw, "normal", admin=True)["machines"][1]
        assert tower["state"] != "configuring"

    def test_reason_categories(self) -> None:
        assert failure_reason(
            "download failed on 3 tries (desktop, tower, desktop): stalled: the archive "
            "ended early"
        ) == REASON_DOWNLOAD, "checked first, whatever words follow"
        assert failure_reason(
            "the library cannot read its own copy of this archive: [Errno 5] at byte 0"
        ) == REASON_ARCHIVE
        assert failure_reason("BadZipFile: File is not a zip file") == REASON_ARCHIVE
        assert failure_reason("3 pages short of the .mokuro") == REASON_ARCHIVE
        assert failure_reason("processor tower disconnected") == REASON_INTERRUPTED
        assert failure_reason("RuntimeError: HIP out of memory") == REASON_ENGINE
        assert failure_reason(None) == REASON_RETRY
        assert failure_reason("") == REASON_RETRY


# --- through HTTP ---------------------------------------------------------------


def _auth(username: str, password: str) -> str:
    return "Basic " + base64.b64encode(f"{username}:{password}".encode()).decode()


def _get(
    app: QueueAPI, headers: dict[str, str] | None = None, ip: str = "10.0.0.1"
) -> tuple[str, dict[str, str], bytes]:
    environ: dict[str, Any] = {
        "REQUEST_METHOD": "GET", "PATH_INFO": "/queue/api/status", "QUERY_STRING": "",
        "wsgi.input": io.BytesIO(b""), "wsgi.errors": io.StringIO(), "REMOTE_ADDR": ip,
    }
    for key, value in (headers or {}).items():
        environ["HTTP_" + key.upper().replace("-", "_")] = value
    captured: dict[str, Any] = {}

    def start_response(status: str, hdrs: list[tuple[str, str]]) -> None:
        captured["status"] = status
        captured["headers"] = dict(hdrs)

    body = b"".join(app(environ, start_response))
    return captured["status"], captured["headers"], body


def _write_state(storage: Path) -> None:
    (storage / ".ocr-progress.json").write_text(json.dumps({
        "active": True,
        "jobs": [
            _card(),
            _card(volume="Volume 07", slot=1, machine="tower", processor=LABEL),
        ],
    }), encoding="utf-8")
    (storage / ".ocr-failures.json").write_text(json.dumps({
        "k": {"series": "Series C", "volume": "Volume 03", "generation": "nova",
              "error": ERROR_TEXT, "attempts": 2, "log_file": LOG_PATH},
    }), encoding="utf-8")


@pytest.fixture(autouse=True)
def fresh_limiter(monkeypatch: pytest.MonkeyPatch) -> None:
    """Each test its own login limiter: failures must not leak between tests."""
    from mokuro_bunko.security import AuthAttemptLimiter

    monkeypatch.setattr(queue_api_module, "AUTH_RATE_LIMITER", AuthAttemptLimiter())


@pytest.fixture
def db(tmp_path: Path) -> Database:
    database = Database(tmp_path / "users.db")
    database.create_user("root", "rootpass1", "admin")
    database.create_user("reader", "readerpass1", "registered")
    return database


class TestHttpRedaction:
    def _app(self, storage: Path, db: Database, display: str = "normal") -> QueueAPI:
        return QueueAPI(
            lambda e, s: [], storage_base_path=str(storage), database=db,
            queue_config=QueueConfig(display=display),
        )

    def test_anonymous_and_registered_viewers_get_the_redacted_payload(
        self, tmp_path: Path, db: Database
    ) -> None:
        _write_state(tmp_path)
        app = self._app(tmp_path, db)
        for headers in (None, {"Authorization": _auth("reader", "readerpass1")}):
            status, _, body = _get(app, headers)
            assert status.startswith("200")
            text = body.decode()
            for secret in ("CUDA", "/srv", "RTX 4090", "log_file", '"error"'):
                assert secret not in text, secret
            data = json.loads(text)
            assert data["failed"][0]["reason"] == REASON_ENGINE
            assert [m["name"] for m in data["machines"]] == ["this server", "machine 1"]

    def test_an_admin_gets_the_raw_error_and_the_hardware(
        self, tmp_path: Path, db: Database
    ) -> None:
        _write_state(tmp_path)
        app = self._app(tmp_path, db)
        status, _, body = _get(app, {"Authorization": _auth("root", "rootpass1")})
        assert status.startswith("200")
        data = json.loads(body)
        assert data["failed"][0]["error"] == ERROR_TEXT
        assert data["failed"][0]["log_file"] == LOG_PATH
        assert data["machines"][1]["label"] == LABEL

    def test_a_wrong_password_is_served_exactly_as_no_header(
        self, tmp_path: Path, db: Database
    ) -> None:
        """No oracle beyond the login page's: same body, same ETag family."""
        _write_state(tmp_path)
        app = self._app(tmp_path, db)
        _, anon_headers, anon_body = _get(app, None, ip="10.0.0.9")
        _, wrong_headers, wrong_body = _get(
            app, {"Authorization": _auth("root", "nope")}, ip="10.0.0.9"
        )
        _, other_headers, other_body = _get(
            app, {"Authorization": "Basic !!!not-base64"}, ip="10.0.0.9"
        )
        assert wrong_body == anon_body == other_body
        assert wrong_headers["ETag"] == anon_headers["ETag"] == other_headers["ETag"]
        assert "CUDA" not in wrong_body.decode()
        # ...and the page is told to forget the stored login.
        assert wrong_headers.get("X-Queue-Auth") == "failed"
        assert other_headers.get("X-Queue-Auth") == "failed"
        assert "X-Queue-Auth" not in anon_headers

    def test_cache_and_vary_headers(self, tmp_path: Path, db: Database) -> None:
        _, headers, _ = _get(self._app(tmp_path, db))
        assert headers["Cache-Control"] == "private, no-cache"
        assert headers["Vary"] == "Authorization"

    def _counting(self, db: Database, monkeypatch: pytest.MonkeyPatch) -> list[str]:
        calls: list[str] = []
        real = db.authenticate_user

        def counting(username: str, password: str) -> Any:
            calls.append(username)
            return real(username, password)

        monkeypatch.setattr(db, "authenticate_user", counting)
        return calls

    def test_one_bcrypt_check_per_header_not_per_poll(
        self, tmp_path: Path, db: Database, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        calls = self._counting(db, monkeypatch)
        app = self._app(tmp_path, db)
        for header in (_auth("root", "rootpass1"), _auth("root", "stale-password")):
            for _ in range(20):
                _get(app, {"Authorization": header})
        assert len(calls) == 2, "one check for the good header, one for the stale one"

    def test_an_account_change_drops_the_cache(
        self, tmp_path: Path, db: Database, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _write_state(tmp_path)
        app = self._app(tmp_path, db)
        header = {"Authorization": _auth("root", "rootpass1")}
        assert "CUDA" in _get(app, header)[2].decode()
        db.update_user_role("root", "registered")
        assert "CUDA" not in _get(app, header)[2].decode(), "demoted at once"
        db.update_user_role("root", "admin")
        db.update_user_password("root", "newpass123")
        _, headers, body = _get(app, header)
        assert "CUDA" not in body.decode() and headers.get("X-Queue-Auth") == "failed"

    def test_guesses_go_through_the_login_rate_limit(
        self, tmp_path: Path, db: Database, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        calls = self._counting(db, monkeypatch)
        _write_state(tmp_path)
        app = self._app(tmp_path, db)
        for n in range(15):
            _get(app, {"Authorization": _auth("root", f"guess-{n}")}, ip="10.0.0.7")
        assert len(calls) == 10, "the limiter stops checking after 10 failures"
        # Blocked: even the right password is not checked, and says nothing.
        _, headers, body = _get(
            app, {"Authorization": _auth("root", "rootpass1")}, ip="10.0.0.7"
        )
        assert len(calls) == 10
        assert "CUDA" not in body.decode()
        assert headers.get("X-Queue-Auth") == "limited"
        # Keyed like the middleware: another address is not blocked.
        _, _, body = _get(app, {"Authorization": _auth("root", "rootpass1")}, ip="10.0.0.8")
        assert "CUDA" in body.decode()


class TestEtag:
    def _worker_app(self, tmp_path: Path) -> tuple[QueueAPI, OCRWorker, list[int]]:
        rows = parse_generation_list([{"name": "nova", "engine": "hayai-nova", "primary": True}])
        _library(tmp_path, S=["A", "B"])
        worker = OCRWorker(storage_path=tmp_path, generations=rows,
                           page_count_lookup=lambda cbz: 100)
        worker.rates.record_volume(rows[0].id, 100, 10.0)
        control = OcrControl()
        control.worker = worker
        plans: list[int] = []
        real_plan = worker.queue_plan

        def counting_plan(*args: Any, **kwargs: Any) -> Any:
            plans.append(1)
            return real_plan(*args, **kwargs)

        worker.queue_plan = counting_plan  # type: ignore[method-assign]
        app = QueueAPI(lambda e, s: [], storage_base_path=str(tmp_path),
                       generations=rows, ocr_control=control,
                       queue_config=QueueConfig())
        return app, worker, plans

    def test_an_unchanged_poll_is_a_304_that_recomputes_nothing(self, tmp_path: Path) -> None:
        app, _worker, plans = self._worker_app(tmp_path)
        status, headers, body = _get(app)
        assert status.startswith("200") and body
        etag = headers["ETag"]
        assert app.builds == 1 and len(plans) == 1

        for _ in range(500):
            status, headers, body = _get(app, {"If-None-Match": etag})
            assert status.startswith("304")
            assert body == b""
            assert headers["ETag"] == etag
        assert app.builds == 1, "no payload was serialized for an unchanged poll"
        assert len(plans) == 1, "no queue was planned for an unchanged poll"

    def test_a_new_viewer_is_served_the_cached_body(self, tmp_path: Path) -> None:
        app, _worker, plans = self._worker_app(tmp_path)
        first = _get(app)
        second = _get(app)
        assert second[2] == first[2]
        assert app.builds == 1 and len(plans) == 1

    def test_levels_and_admins_are_cached_apart(self, tmp_path: Path) -> None:
        app, _worker, _plans = self._worker_app(tmp_path)
        _, normal, _ = _get(app)
        app._queue_config.display = "minimal"  # type: ignore[union-attr]
        status, minimal, body = _get(app, {"If-None-Match": normal["ETag"]})
        assert status.startswith("200"), "a level change is never a 304"
        assert minimal["ETag"] != normal["ETag"]
        assert json.loads(body)["level"] == "minimal"

    def test_a_page_event_is_a_new_version(self, tmp_path: Path) -> None:
        app, worker, plans = self._worker_app(tmp_path)
        job = (tmp_path / "library" / "S" / "A.cbz", worker.generations[0].id)
        worker.begin_ocr_job(job, worker.generations[0], slot=0)
        _, headers, _ = _get(app)
        worker._set_active_progress(job, {"done_pages": 3, "total_pages": 100})
        status, fresh, body = _get(app, {"If-None-Match": headers["ETag"]})
        assert status.startswith("200")
        assert fresh["ETag"] != headers["ETag"]
        assert json.loads(body)["machines"][0]["jobs"][0]["done_pages"] == 3
        assert app.builds == 2 and len(plans) == 2

    def test_a_rebuild_that_changes_nothing_keeps_the_etag(self, tmp_path: Path) -> None:
        app, worker, _plans = self._worker_app(tmp_path)
        _, headers, _ = _get(app)
        worker.queue_state.bump()  # a spurious bump: nothing shown moved
        status, again, _ = _get(app, {"If-None-Match": headers["ETag"]})
        assert app.builds == 2
        assert status.startswith("304") and again["ETag"] == headers["ETag"]


# --- the version moves on every event kind ---------------------------------------


def _library(storage: Path, **series: list[str]) -> None:
    (storage / "inbox").mkdir(exist_ok=True)
    for name, volumes in series.items():
        folder = storage / "library" / name
        folder.mkdir(parents=True, exist_ok=True)
        for volume in volumes:
            with zipfile.ZipFile(folder / f"{volume}.cbz", "w") as zf:
                zf.writestr("page_000.jpg", b"fake image data")


class TestVersionBumps:
    @pytest.fixture
    def worker(self, tmp_path: Path) -> OCRWorker:
        rows = parse_generation_list([{"name": "nova", "engine": "hayai-nova", "primary": True}])
        _library(tmp_path, S=["A"])
        return OCRWorker(storage_path=tmp_path, generations=rows)

    def _moves(self, worker: OCRWorker, action: Callable[[], Any]) -> bool:
        before = worker.queue_state.value
        action()
        return worker.queue_state.value > before

    def test_each_event_kind(self, worker: OCRWorker, tmp_path: Path) -> None:
        row = worker.generations[0]
        cbz = tmp_path / "library" / "S" / "A.cbz"
        job = (cbz, row.id)
        assert self._moves(worker, lambda: worker.begin_ocr_job(job, row, slot=0)), "claim"
        assert self._moves(
            worker, lambda: worker._set_active_progress(job, {"done_pages": 1})
        ), "page event"
        assert self._moves(worker, lambda: worker._clear_active_progress(job)), "volume done"
        assert self._moves(
            worker, lambda: worker._record_ocr_failure(cbz, row, OcrFailure("boom"))
        ), "volume failed"
        assert self._moves(worker, lambda: worker.apply_settings(list(worker.generations))), (
            "settings change"
        )

    def test_the_pending_list_moving_and_only_that(
        self, worker: OCRWorker, tmp_path: Path
    ) -> None:
        worker.refresh_pending(max_age=0)
        assert not self._moves(worker, lambda: worker.refresh_pending(max_age=0)), (
            "recomputing the same list is not a change"
        )
        _library(tmp_path, S=["B"])
        assert self._moves(worker, lambda: worker.refresh_pending(max_age=0)), "new upload"

    def test_a_fresh_cache_costs_no_recomputation(
        self, worker: OCRWorker, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker.refresh_pending()
        calls: list[int] = []
        monkeypatch.setattr(worker, "_upcoming_ocr_jobs", lambda **k: calls.append(1) or [])
        for _ in range(100):
            worker.refresh_pending()
        assert calls == []

    def test_a_claim_updates_the_pending_list_without_a_walk(
        self, worker: OCRWorker, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _library(tmp_path, S=["B"])
        worker.refresh_pending(max_age=0)
        assert [e["volume"] for e in worker.last_pending()] == ["A", "B"]
        calls: list[int] = []
        monkeypatch.setattr(worker, "_upcoming_ocr_jobs", lambda **k: calls.append(1) or [])
        job = (tmp_path / "library" / "S" / "A.cbz", worker.generations[0].id)
        with worker._lock:
            worker._bump_queue_generation(claimed=job)
        assert [e["volume"] for e in worker.last_pending()] == ["B"]
        with worker._lock:
            worker._bump_queue_generation(keep_cache=True)  # it finished
        assert [e["volume"] for e in worker.last_pending()] == ["B"]
        assert calls == [], "a build after a claim walked the library"

    def test_the_control_shares_one_counter(self, worker: OCRWorker) -> None:
        control = OcrControl()
        control.worker = worker
        assert worker.queue_state is control.queue_state


# --- speed ---------------------------------------------------------------------


class TestSpeed:
    def test_rate_model_names_the_machines_with_recent_evidence(self, tmp_path: Path) -> None:
        rates = RateModel(tmp_path)
        rates.record_volume("g-2", 100, 100.0)
        rates.record_volume("g-2@tower", 100, 50.0)
        rates.record_volume("g-9@tower", 100, 50.0)
        assert sorted(rates.machines_with_evidence("g-2", within=60)) == ["local", "tower"]
        assert rates.machines_with_evidence("g-2", within=-1) == []

    def test_per_machine_real_throughput_and_combined(self, tmp_path: Path) -> None:
        rows = parse_generation_list([
            {"name": "mokuro", "engine": "mokuro", "primary": True},
            {"name": "nova", "engine": "hayai-nova"},
        ])
        _library(tmp_path, S=["A"])
        worker = OCRWorker(storage_path=tmp_path, generations=rows)
        nova = rows[1].id
        # This server read 60 pages a minute; tower twice that.
        worker.rates.record_volume(nova, 100, 100.0)
        worker.rates.record_volume(f"{nova}@tower", 100, 50.0)
        # The ETA model's live rate on a card is NOT a speed anyone is shown.
        running = [{"generation_id": nova, "machine": "tower", "status": "running",
                    "rate_pages_per_second": 2.5}]
        (entry,) = worker.speed_report(running)
        assert entry["generation"] == "nova"
        by_name = {m["machine"]: m for m in entry["machines"]}
        assert by_name["tower"] == {"machine": "tower", "pages_per_minute": 120.0,
                                    "volumes": 1, "lanes": 1, "working": True}
        assert by_name["local"] == {"machine": "local", "pages_per_minute": 60.0,
                                    "volumes": 1, "lanes": 0, "working": False}
        # Combined is what the lanes READING it now deliver together.
        assert entry["combined_pages_per_minute"] == 120.0
        assert "average_pages_per_minute" not in entry

    def test_combined_counts_reading_lanes_only(self, tmp_path: Path) -> None:
        rows = parse_generation_list([{"name": "nova", "engine": "hayai-nova", "primary": True}])
        _library(tmp_path, S=["A"])
        worker = OCRWorker(storage_path=tmp_path, generations=rows)
        nova = rows[0].id
        worker.rates.record_volume(f"{nova}@tower", 100, 50.0)
        running = [
            {"generation_id": nova, "machine": "tower", "status": "running"},
            {"generation_id": nova, "machine": "tower", "status": "running"},
            # On deck / loading: delivers nothing yet.
            {"generation_id": nova, "machine": "tower", "status": "starting"},
        ]
        (entry,) = worker.speed_report(running)
        assert entry["machines"][0]["lanes"] == 2
        assert entry["combined_pages_per_minute"] == 240.0
        (idle,) = worker.speed_report([])
        assert idle["combined_pages_per_minute"] is None

    def test_no_displayed_speed_is_the_fitted_slope(self, tmp_path: Path) -> None:
        """A machine whose ETA fit says 70 pages/s but whose volumes really
        came out at 25 pages/s is shown as 25 -- everywhere."""
        rows = parse_generation_list([{"name": "nova", "engine": "hayai-nova", "primary": True}])
        _library(tmp_path, S=["A"])
        worker = OCRWorker(storage_path=tmp_path, generations=rows)
        nova = rows[0].id
        key = f"{nova}@tower"
        # seconds = latency + pages / 70: a big fixed cost per volume, so the
        # fitted marginal slope is 70 pages/s while pages over seconds is 25.
        latency = (200 / 25 - 200 / 70) / 2
        for pages in (50, 150):
            worker.rates.record_volume(key, pages, latency + pages / 70)
        fitted = worker.rates.rate_on(nova, key)
        assert fitted is not None and fitted.pages_per_second == pytest.approx(70.0)
        real = worker.rates.throughput(key)
        assert real is not None and real.pages_per_second == pytest.approx(25.0)

        running = [{"generation_id": nova, "machine": "tower", "status": "running",
                    "rate_pages_per_second": fitted.pages_per_second}]
        (entry,) = worker.speed_report(running)
        assert entry["machines"][0]["pages_per_minute"] == pytest.approx(1500.0)
        assert entry["combined_pages_per_minute"] == pytest.approx(1500.0)
        raw = {"current_jobs": [dict(running[0], series="S", volume="V",
                                     generation="nova", slot=0)],
               "speed": worker.speed_report(running), "pending_ocr": []}
        detailed = shape_status(raw, "detailed", admin=True)
        assert detailed["speed"][0]["pages_per_minute"] == pytest.approx(1500.0)
        job = detailed["machines"][0]["jobs"][0]
        assert job["throughput_pages_per_minute"] == pytest.approx(1500.0)
        for level in ("minimal", "normal", "detailed"):
            text = json.dumps(shape_status(raw, level, admin=True))
            assert "4200" not in text and "70.0" not in text, level

    def test_a_row_nothing_has_run_is_left_out(self, tmp_path: Path) -> None:
        rows = parse_generation_list([{"name": "mokuro", "engine": "mokuro", "primary": True}])
        _library(tmp_path, S=["A"])
        worker = OCRWorker(storage_path=tmp_path, generations=rows)
        assert worker.speed_report([]) == []

    def test_the_status_payload_carries_it_at_detailed_only(self, tmp_path: Path) -> None:
        rows = parse_generation_list([{"name": "nova", "engine": "hayai-nova", "primary": True}])
        _library(tmp_path, S=["A"])
        worker = OCRWorker(storage_path=tmp_path, generations=rows)
        worker.rates.record_volume(rows[0].id, 120, 60.0)
        control = OcrControl()
        control.worker = worker
        config = QueueConfig(display="minimal")
        app = QueueAPI(lambda e, s: [], storage_base_path=str(tmp_path),
                       generations=rows, ocr_control=control, queue_config=config)
        for level in ("minimal", "normal"):
            config.display = level
            assert "speed" not in json.loads(_get(app)[2]), level
        config.display = "detailed"
        # Nothing is reading the row right now: no line for it.
        assert json.loads(_get(app)[2])["speed"] == []


# --- config ----------------------------------------------------------------------


class TestConfig:
    def test_default_and_a_bad_value_falls_back(self, caplog: pytest.LogCaptureFixture) -> None:
        assert QueueConfig().display == "normal"
        with caplog.at_level("WARNING"):
            assert QueueConfig(display="verbose").display == "normal"
        assert "queue.display" in caplog.text

    def test_round_trip_and_dotted_key(self) -> None:
        config = Config.from_dict({"queue": {"display": "detailed"}})
        assert config.queue.display == "detailed"
        assert config.to_dict()["queue"]["display"] == "detailed"
        set_by_dotted_key(config, "queue.display", "minimal")
        assert config.queue.display == "minimal"
        with pytest.raises(ValueError):
            set_by_dotted_key(config, "queue.display", "loud")

    def test_the_level_is_read_live(self, tmp_path: Path) -> None:
        cfg = QueueConfig()
        app = QueueAPI(lambda e, s: [], storage_base_path=str(tmp_path), queue_config=cfg)
        assert json.loads(_get(app)[2])["level"] == "normal"
        cfg.display = "detailed"
        time.sleep(0)  # nothing else: the very next poll
        assert json.loads(_get(app)[2])["level"] == "detailed"


# --- visitors' machine names -------------------------------------------------------


class TestAliases:
    def _raw(self) -> dict[str, Any]:
        raw = _raw()
        raw["processing_hold"] = {
            "reason": "no-processor", "since": 1.0,
            "last": {"name": "tower", "disconnected_at": 2.0}, "extra": "dropped",
        }
        raw["paused_for_benchmark"] = {
            "key": "draft-secret", "generation": "nova", "queued": 1, "processor": "tower",
        }
        raw["skipped_missing_pages"] = [
            {"series": "S", "volume": "V", "missing_pages": 2, "page_count": 10,
             "generations": ["nova"], "path": "/srv/library/S/V.cbz"},
        ]
        return raw

    def test_a_visitor_never_gets_a_processor_name(self) -> None:
        names = PublicNames()
        names.assign("desktop")
        names.assign("tower")
        for level in ("minimal", "normal", "detailed"):
            data = shape_status(self._raw(), level, admin=False, public_names=names)
            assert "tower" not in json.dumps(data), level
            assert [m["name"] for m in data["machines"]] == ["this server", "machine 2"]
            assert data["processing_hold"] == {
                "reason": "no-processor", "since": 1.0,
                "last": {"name": "machine 2", "disconnected_at": 2.0},
            }
            # A draft's "generation" is its bench key: a visitor gets neither.
            assert data["paused_for_benchmark"] == {"queued": 1, "processor": "machine 2"}
        skipped = shape_status(self._raw(), "normal", admin=False)["skipped_missing_pages"]
        assert skipped == [{"series": "S", "volume": "V", "missing_pages": 2,
                            "page_count": 10, "generations": ["nova"]}]

    def test_aliases_are_stable_and_an_explicit_public_name_wins(self) -> None:
        names = PublicNames()
        assert names.assign("tower") == "machine 1"
        assert names.assign("desktop", "the big box") == "the big box"
        assert names("tower") == "machine 1"
        assert names.assign("tower") == "machine 1", "a reconnect keeps its alias"
        assert names("never-registered") == "machine 2"
        assert names("local") == "this server"

    def test_an_admin_gets_the_real_names_and_the_key(self) -> None:
        data = shape_status(self._raw(), "normal", admin=True, public_names=PublicNames())
        assert [m["name"] for m in data["machines"]] == ["this server", "tower"]
        assert data["processing_hold"]["last"]["name"] == "tower"
        assert data["paused_for_benchmark"]["key"] == "draft-secret"
        assert data["paused_for_benchmark"]["processor"] == "tower"

    def test_the_registry_numbers_processors_in_registration_order(self) -> None:
        from mokuro_bunko.ocr.remote.registry import ProcessorRegistry

        registry = ProcessorRegistry(local_name="this server")
        for name, public in (("desktop", None), ("tower", "big box"), ("laptop", None)):
            registry.register(username=name, name=name, host={}, catalog={},
                              max_sessions=1, public_name=public)
        assert registry.public_names("desktop") == "machine 1"
        assert registry.public_names("tower") == "big box"
        assert registry.public_names("laptop") == "machine 2"
        control = OcrControl()
        control.remote = registry
        app = QueueAPI(lambda e, s: [], storage_base_path="/nonexistent", ocr_control=control)
        assert app.public_names is registry.public_names

    def test_processor_yaml_public_name(self, tmp_path: Path) -> None:
        from mokuro_bunko.processor.config import ProcessorConfigError, load_processor_config

        base = ("library:\n  url: https://l.example\n  username: u\n  password: p\n"
                "processor:\n  name: tower\n")
        path = tmp_path / "p.yaml"
        path.write_text(base + "  public_name: big box\n", encoding="utf-8")
        assert load_processor_config(path).processor.public_name == "big box"
        path.write_text(base, encoding="utf-8")
        assert load_processor_config(path).processor.public_name is None
        path.write_text(base + "  public_name: " + "x" * 65 + "\n", encoding="utf-8")
        with pytest.raises(ProcessorConfigError, match="public_name"):
            load_processor_config(path)


# --- freshness without request-thread walks ------------------------------------------


def _wait_refreshed(app: QueueAPI) -> None:
    deadline = time.monotonic() + 5
    while app._refreshing and time.monotonic() < deadline:
        time.sleep(0.01)


class TestFreshness:
    def _app(self, tmp_path: Path) -> tuple[QueueAPI, OCRWorker, OcrControl]:
        rows = parse_generation_list([{"name": "nova", "engine": "hayai-nova", "primary": True}])
        _library(tmp_path, S=["A"])
        worker = OCRWorker(storage_path=tmp_path, generations=rows)
        control = OcrControl()
        control.worker = worker
        app = QueueAPI(lambda e, s: [], storage_base_path=str(tmp_path),
                       generations=rows, ocr_control=control, queue_config=QueueConfig())
        return app, worker, control

    def test_a_changed_missing_pages_list_is_not_hidden_behind_304s(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        app, _worker, control = self._app(tmp_path)
        skipped: list[dict[str, Any]] = []
        monkeypatch.setattr(control, "skipped_missing_pages", lambda: list(skipped))
        _, headers, body = _get(app)
        assert json.loads(body)["skipped_missing_pages"] == []
        # A compile finishes and reports a short volume.
        skipped.append({"series": "S", "volume": "A", "missing_pages": 3,
                        "generations": ["nova"]})
        app.invalidate_skipped()
        _get(app, {"If-None-Match": headers["ETag"]})  # kicks the refresh
        _wait_refreshed(app)
        status, fresh, body = _get(app, {"If-None-Match": headers["ETag"]})
        assert status.startswith("200"), "the new list is a new version"
        assert json.loads(body)["skipped_missing_pages"][0]["missing_pages"] == 3

    def test_a_slow_refresh_never_blocks_a_poll(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        app, _worker, control = self._app(tmp_path)
        _, headers, _ = _get(app)
        release = threading.Event()
        monkeypatch.setattr(control, "refresh_pending", lambda: release.wait(10))
        app._refreshed_at = float("-inf")
        try:
            started = time.monotonic()
            for _ in range(5):
                status, _, _ = _get(app, {"If-None-Match": headers["ETag"]})
                assert status.startswith("304")
            assert time.monotonic() - started < 0.5, "polls waited on a library walk"
            assert app._refreshing, "the slow refresh is still running, off the request"
        finally:
            release.set()
            _wait_refreshed(app)

    def test_a_slow_build_serves_everyone_else_the_last_body(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        app, worker, _control = self._app(tmp_path)
        _, headers, first = _get(app)
        release = threading.Event()
        real = app.raw_status

        def slow_raw() -> dict[str, Any]:
            release.wait(10)
            return real()

        monkeypatch.setattr(app, "raw_status", slow_raw)
        worker.queue_state.bump()  # something changed: a build is due
        builder = threading.Thread(target=_get, args=(app,))
        builder.start()
        deadline = time.monotonic() + 5
        while not app._building and time.monotonic() < deadline:
            time.sleep(0.005)
        try:
            started = time.monotonic()
            status, again, body = _get(app, {"If-None-Match": headers["ETag"]})
            assert time.monotonic() - started < 0.5
            assert status.startswith("304") and again["ETag"] == headers["ETag"]
            status, _, body = _get(app)
            assert status.startswith("200") and body == first
            assert app.builds == 2, "one build in flight, not one per poll"
        finally:
            release.set()
            builder.join(5)

    def test_a_processor_connecting_is_a_new_version(self, tmp_path: Path) -> None:
        from mokuro_bunko.ocr.remote.registry import ProcessorRegistry

        app, _worker, control = self._app(tmp_path)
        registry = ProcessorRegistry(local_name="this server")
        control.remote = registry
        _get(app)
        builds = app.builds
        registry.register(username="tower", name="tower", host={}, catalog={},
                          max_sessions=1)
        _get(app)
        # Lanes and ETAs may move: rebuilt. (Nothing is pending here, so the
        # body -- and so its ETag -- comes out the same.)
        assert app.builds == builds + 1

    def test_the_version_moves_after_the_file_is_written(self, tmp_path: Path) -> None:
        _app, worker, _control = self._app(tmp_path)
        seen: list[bool] = []
        real = worker.queue_state.bump

        def bump() -> int:
            seen.append((tmp_path / ".ocr-progress.json").exists())
            return real()

        worker.queue_state.bump = bump  # type: ignore[method-assign]
        row = worker.generations[0]
        worker.begin_ocr_job((tmp_path / "library" / "S" / "A.cbz", row.id), row, slot=0)
        assert seen == [True]



class TestRegisterPublicName:
    def _register(self, registry: Any, extra: dict[str, Any]) -> str:
        from mokuro_bunko.ocr.remote.library_api import ProcessorAPI

        api = ProcessorAPI(lambda e, s: [], registry)
        body = json.dumps({"protocol": PROTOCOL_VERSION, "name": "tower", **extra}).encode()
        environ = {
            "REQUEST_METHOD": "POST", "PATH_INFO": "/_processor/register",
            "CONTENT_LENGTH": str(len(body)), "wsgi.input": io.BytesIO(body),
            "mokuro.role": "processor", "mokuro.username": "tower",
        }
        statuses: list[str] = []
        b"".join(api(environ, lambda status, headers: statuses.append(status)))
        return statuses[0]

    def test_it_is_sent_at_register_and_checked_like_the_name(self) -> None:
        from mokuro_bunko.ocr.remote.registry import ProcessorRegistry

        registry = ProcessorRegistry(local_name="this server")
        assert self._register(registry, {"public_name": 5}).startswith("400")
        assert self._register(registry, {"public_name": "this server"}).startswith("400")
        assert self._register(registry, {"public_name": " big box " + "x" * 80}).startswith("200")
        (entry,) = [e for e in registry.entries() if not e.local]
        assert entry.public_name is not None and len(entry.public_name) == 64
        assert registry.public_names("tower") == entry.public_name


class TestLeftovers:
    def test_a_saved_rows_name_is_sent_a_drafts_is_not(self) -> None:
        raw = _raw()
        raw["paused_for_benchmark"] = {"key": "g-2", "generation": "nova", "queued": 0,
                                       "processor": "local"}
        data = shape_status(raw, "normal", admin=False)
        assert data["paused_for_benchmark"] == {"generation": "nova", "queued": 0,
                                                "processor": "local"}
        raw["paused_for_benchmark"] = {"key": "draft-abc", "generation": "draft-abc",
                                       "queued": 0, "processor": "local"}
        assert "draft-abc" not in json.dumps(shape_status(raw, "normal", admin=False))
        admin = shape_status(raw, "normal", admin=True)["paused_for_benchmark"]
        assert admin["key"] == admin["generation"] == "draft-abc"

    def test_the_pipeline_is_rebuilt_field_by_field(self) -> None:
        raw = _raw()
        raw["current_jobs"][0]["pipeline"] = {
            "verdict": "widen detect", "bottleneck": "detect", "host": "tower.lan",
            "stages": [{
                "key": "detect", "name": "detection", "device": "gpu:0", "workers": 2,
                "fused": False, "busy_pct": 90.0, "blocked_pct": 1.0, "starved_pct": 2.0,
                "gpu_name": "RTX 4090", "path": "/srv/models",
                "queue": {"name": "detect->engine", "capacity": 4, "mean_depth": 1.5,
                          "max_depth": 4, "secret": "x"},
            }],
        }
        for admin in (False, True):
            pipeline = shape_status(raw, "detailed", admin=admin)["machines"][0]["jobs"][0][
                "pipeline"
            ]
            assert pipeline == {
                "verdict": "widen detect", "bottleneck": "detect",
                "stages": [{
                    "key": "detect", "name": "detection", "device": "gpu:0", "workers": 2,
                    "fused": False, "busy_pct": 90.0, "blocked_pct": 1.0,
                    "starved_pct": 2.0,
                    "queue": {"name": "detect->engine", "capacity": 4, "mean_depth": 1.5,
                              "max_depth": 4},
                }],
            }

    def test_a_rescan_never_pairs_a_new_count_with_an_old_snapshot(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from mokuro_bunko.library_index import LibraryIndexCache

        _library(tmp_path, S=["A"])
        index = LibraryIndexCache(tmp_path / "library", ttl=0.0)
        real_scan = index._scan_library
        injected: list[bool] = []

        def scan_then_rescan_elsewhere() -> Any:
            snapshot = real_scan()
            if not injected:
                # Another thread's rescan lands between this scan and its
                # publication.
                injected.append(True)
                index.get_snapshot()
            return snapshot

        monkeypatch.setattr(index, "_scan_library", scan_then_rescan_elsewhere)
        snapshot, scans = index.get_snapshot_counted()
        with index._lock:
            assert index._snapshot is snapshot
            assert index.scans == scans, "the count names the snapshot returned"


class TestIdle:
    def test_an_idle_queue_with_processors_connected_holds_still(self, tmp_path: Path) -> None:
        """The live finding: on an idle queue with two processors connected the
        version climbed every few seconds (each scan ticked it twice) and every
        viewer got a full 200 each time. 30 simulated seconds of scans,
        heartbeats and refreshes must now change no ETag and rebuild at most
        ONCE (the first build)."""
        from mokuro_bunko.ocr.remote.registry import ProcessorRegistry

        rows = parse_generation_list([{"name": "nova", "engine": "hayai-nova", "primary": True}])
        _library(tmp_path, S=["A"])
        (tmp_path / "library" / "S" / "A.mokuro").write_text("{}", encoding="utf-8")
        registry = ProcessorRegistry(local_name="this server")
        worker = OCRWorker(storage_path=tmp_path, generations=rows, remote=registry,
                           local_processing=False)
        entries = [
            registry.register(username=n, name=n, host={}, catalog={"engines": ["hayai-nova"]},
                              max_sessions=1)
            for n in ("tower", "desktop")
        ]
        control = OcrControl()
        control.worker = worker
        control.remote = registry
        app = QueueAPI(lambda e, s: [], storage_base_path=str(tmp_path), generations=rows,
                       ocr_control=control, queue_config=QueueConfig())
        worker._running = True
        _, first, _ = _get(app)
        etags = set()
        for _second in range(30):
            worker._scan_ocr_once()                       # the scan loop's tick
            for entry in entries:                          # the heartbeats
                entry.last_seen = time.time()
            app._refreshed_at = float("-inf")              # the refresher is due
            app._refresh()
            worker.refresh_pending(max_age=0)
            _, headers, _ = _get(app, {"If-None-Match": first["ETag"]})
            etags.add(headers["ETag"])
        assert etags == {first["ETag"]}
        assert app.builds == 1


class TestFixedCards:
    def test_a_connected_idle_machine_keeps_its_entry(self) -> None:
        raw = _raw()
        raw["connected_machines"] = [
            {"machine": "local", "slots": 1},
            {"machine": "desktop", "slots": 2},
            {"machine": "tower", "slots": 1},
        ]
        for level in ("minimal", "normal", "detailed"):
            data = shape_status(raw, level, admin=True)
            by_name = {m["name"]: m for m in data["machines"]}
            assert [m["name"] for m in data["machines"]] == ["this server", "desktop", "tower"]
            assert by_name["desktop"]["state"] == "idle"
            assert by_name["desktop"]["jobs"] == []
            assert by_name["desktop"]["slots"] == 2
            assert by_name["tower"]["state"] == "running"

    def test_states(self) -> None:
        from mokuro_bunko.queue.shape import job_state

        assert job_state({"status": "running"}) == "running"
        assert job_state({"status": "finalizing"}) == "running"
        assert job_state({"status": "starting", "startup_seconds": 12}) == "loading"
        assert job_state({"status": "starting", "startup_seconds": None}) == "waiting"

    def test_minimal_sends_at_most_ten_pending(self) -> None:
        raw = _raw()
        raw["pending_ocr"] = [dict(raw["pending_ocr"][0], volume=f"V{n}") for n in range(25)]
        data = shape_status(raw, "minimal", admin=False)
        assert len(data["pending"]) == 10
        assert data["pending_count"] == 25

