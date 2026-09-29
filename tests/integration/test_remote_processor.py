"""A real `processor serve` process against a real library server.

Both ends run the fake runner (`tests/fixtures/fake_runner.py`), so no GPU
and no model weights are needed; what is under test is everything BETWEEN
them -- the handshake, the assignment stream, the archive pull, the event
frames, the sidecar's trip home, cancellation, a disconnect and a
re-registration.

There is no backpressure left to exercise: a processor downloads each
archive WHOLE, at full speed, before its runner reads a byte of it, so no
runner speed can hold the library's socket open. That used to be the whole
problem -- a runner that stopped reading made the library's own 10 s write
timeout cut the archive short -- and the first test below is its
regression: a runner that sleeps three times that timeout before reading
anything, against the real library with its timeout untouched.
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import threading
import time
import zipfile
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any
from unittest.mock import patch

import pytest

from mokuro_bunko.config import Config, OcrConfig, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.generations import parse_generation_list

FAKE_RUNNER = Path(__file__).resolve().parents[1] / "fixtures" / "fake_runner.py"
PASSWORD = "a-long-enough-password"


class _ArchiveFaults:
    """A WSGI wrapper in front of the REAL app: every archive GET, as it went.

    A test can cut the FIRST GET of an archive after N bytes -- raising
    mid-body, so cheroot closes the connection as it does when its own
    write times out -- and, before cutting, replace the file on disk.
    """

    def __init__(self, app: Callable[..., Any]) -> None:
        self.app = app
        self.requests: list[dict[str, Any]] = []
        self.cut_first: dict[str, int] = {}
        self.replace_first: dict[str, tuple[Path, bytes]] = {}

    def __call__(self, environ: dict[str, Any], start_response: Callable[..., Any]) -> Any:
        path = str(environ.get("PATH_INFO") or "")
        if environ.get("REQUEST_METHOD") != "GET" or not path.endswith(".cbz"):
            return self.app(environ, start_response)
        name = path.rsplit("/", 1)[-1]
        record: dict[str, Any] = {
            "name": name, "range": environ.get("HTTP_RANGE"),
            "if_range": environ.get("HTTP_IF_RANGE"),
        }
        self.requests.append(record)

        def respond(status: str, headers: Any, exc_info: Any = None) -> Any:
            record["status"] = int(status.split()[0])
            record["etag"] = dict(headers).get("ETag")
            return start_response(status, headers, exc_info)

        cut = self.cut_first.pop(name, None)
        replacement = self.replace_first.pop(name, None)
        body = self.app(environ, respond)
        if cut is None:
            return body

        def cut_short() -> Iterator[bytes]:
            sent = 0
            try:
                for chunk in body:
                    if sent + len(chunk) >= cut:
                        yield chunk[: cut - sent]
                        if replacement is not None:
                            target, data = replacement
                            staged = target.with_name(target.name + ".new")
                            staged.write_bytes(data)
                            os.replace(staged, target)
                        raise ConnectionAbortedError("the test cuts this GET short")
                    sent += len(chunk)
                    yield chunk
            finally:
                close = getattr(body, "close", None)
                if callable(close):
                    close()

        return cut_short()

    def gets(self, name: str) -> list[dict[str, Any]]:
        return [r for r in self.requests if r["name"] == name]


def _stored_cbz(path: Path, sizes: list[int]) -> bytes:
    """A stored .cbz of random 'pages' of the given sizes; its bytes."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_STORED) as zf:
        for n, size in enumerate(sizes):
            zf.writestr(f"page_{n:03d}.jpg", os.urandom(size))
    return path.read_bytes()


def _free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _volume(library: Path, series: str, volume: str, pages: int = 3) -> Path:
    cbz = library / series / f"{volume}.cbz"
    cbz.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(cbz, "w") as zf:
        for n in range(pages):
            zf.writestr(f"page_{n:03d}.jpg", b"fake image data" * 50)
    cbz.with_suffix(".mokuro").write_text(
        json.dumps({"version": "0.2.5", "title": series, "volume": volume,
                    "volume_uuid": f"uuid-{volume}", "pages": [], "chars": 0}),
        encoding="utf-8",
    )
    return cbz


@pytest.fixture
def library(tmp_path: Path) -> Iterator[dict[str, Any]]:
    """A running library server with OCR on and local processing OFF."""
    from cheroot.wsgi import Server as WSGIServer

    from mokuro_bunko.ocr.watcher import OCRWorker
    from mokuro_bunko.server import create_app

    storage = tmp_path / "library-server"
    for name in ("library", "inbox", "users"):
        (storage / name).mkdir(parents=True)
    rows = parse_generation_list(
        [
            {"name": "mokuro", "engine": "mokuro", "primary": True},
            {"name": "hayai-ctd", "engine": "hayai-nova", "detector": "ctd"},
        ]
    )
    config = Config(
        storage=StorageConfig(base_path=storage),
        ocr=OcrConfig(backend="skip", generations=rows, local_processing=False),
    )
    database = Database(storage / "mokuro.db")
    database.create_user("tower", PASSWORD, "processor")

    _volume(storage / "library", "Alpha", "Volume 1")
    _volume(storage / "library", "Alpha", "Volume 2")

    control = OcrControl()
    app = create_app(config, tmp_path / "config.yaml", ocr_control=control)
    faults = _ArchiveFaults(app)
    worker = OCRWorker(
        storage_path=storage,
        poll_interval=1.0,
        generations=rows,
        engines_python_path=Path(sys.executable),
        concurrency=1,
        sessions=True,
        remote=control.remote,
        local_processing=False,
    )
    control.worker = worker
    assert control.remote is not None
    control.remote.on_drop = worker.processor_disconnected

    port = _free_port()
    # cheroot's own socket timeout (10 s) is left exactly as the library
    # runs with it: it bounds the library's WRITES too, which is what used
    # to cut archives short under a runner that stopped reading.
    server = WSGIServer(("127.0.0.1", port), faults, numthreads=25)
    server.prepare()
    threading.Thread(target=server.serve, daemon=True).start()
    time.sleep(0.5)
    yield {"storage": storage, "port": port, "worker": worker,
           "registry": control.remote, "rows": rows, "faults": faults,
           "server": server}
    worker.stop()
    server.stop()
    for attribute in ("_library_watcher", "_metadata_service", "_propfind_cache"):
        held = getattr(app, attribute, None)
        if held is not None:
            held.stop()


@pytest.fixture
def processor_config(tmp_path: Path, library: dict[str, Any]) -> Path:
    state = tmp_path / "processor-state"
    state.mkdir()
    path = tmp_path / "processor.yaml"
    path.write_text(
        "library:\n"
        f"  url: http://127.0.0.1:{library['port']}\n"
        "  username: tower\n"
        f"  password: {PASSWORD}\n"
        "processor:\n"
        "  name: tower\n"
        "  max_sessions: 1\n"
        f"  storage: {state}\n",
        encoding="utf-8",
    )
    return path


def _spawn(config_path: Path, script: Path) -> subprocess.Popen[str]:
    env = dict(
        os.environ,
        PYTHONUNBUFFERED="1",
        MOKURO_PROCESSOR_RUNNER=str(FAKE_RUNNER),
        MOKURO_PROCESSOR_ENGINES_PYTHON=sys.executable,
        FAKE_RUNNER_SCRIPT=str(script),
    )
    return subprocess.Popen(
        [sys.executable, "-m", "mokuro_bunko", "processor", "serve",
         "--config", str(config_path)],
        env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    )


def _script(tmp_path: Path, **rules: Any) -> Path:
    path = tmp_path / "fake-runner-script.json"
    path.write_text(json.dumps(rules), encoding="utf-8")
    return path


def _wait(predicate: Callable[[], bool], timeout: float = 60.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.25)
    return False


def _names(registry: Any) -> list[str]:
    return sorted(entry.name for entry in registry.connected() if not entry.local)


@pytest.mark.slow
class TestOneProcessorEndToEnd:
    def test_register_session_two_volumes_and_the_sidecars_come_home(
        self, tmp_path: Path, library: dict[str, Any], processor_config: Path
    ) -> None:
        process = _spawn(processor_config, _script(tmp_path, pages=3, page_delay=0.02))
        try:
            registry = library["registry"]
            assert _wait(lambda: _names(registry) == ["tower"]), "it never registered"
            entry = registry.connected()[0]
            assert "hayai-nova" in entry.catalog["engines"]

            worker = library["worker"]
            worker._scan_ocr_once()

            produced = sorted(
                p.name
                for p in (library["storage"] / "library").rglob("*.hayai-ctd.mokuro")
            )
            assert produced == [
                "Volume 1.hayai-ctd.mokuro",
                "Volume 2.hayai-ctd.mokuro",
            ]
            written = json.loads(
                (library["storage"] / "library" / "Alpha" /
                 "Volume 1.hayai-ctd.mokuro").read_text(encoding="utf-8")
            )
            assert written["volume_uuid"] == "uuid-Volume 1", (
                "a remote layer still inherits the volume's own uuid"
            )
            assert written["ocr_engine"]["generation"] == "hayai-ctd"
            assert not (library["storage"] / ".ocr-failures.json").exists()
        finally:
            process.terminate()
            process.wait(timeout=30)

    def test_a_layer_is_run_for_a_volume_that_has_no_mokuro_yet(
        self, tmp_path: Path, library: dict[str, Any], processor_config: Path
    ) -> None:
        """No layer waits for its primary: the processor reads a volume the
        library has no `<Volume>.mokuro` for, and the layer it sends home
        names the volume by the same id the primary will carry."""
        from mokuro_bunko.metadata.reader_compat import deterministic_uuid

        cbz = library["storage"] / "library" / "Alpha" / "Volume 3.cbz"
        with zipfile.ZipFile(cbz, "w") as zf:
            for n in range(3):
                zf.writestr(f"page_{n:03d}.jpg", b"fake image data" * 50)
        process = _spawn(processor_config, _script(tmp_path, pages=3, page_delay=0.02))
        try:
            assert _wait(lambda: _names(library["registry"]) == ["tower"])
            library["worker"]._scan_ocr_once()
            layer = cbz.with_name("Volume 3.hayai-ctd.mokuro")
            assert layer.is_file(), "the layer ran without the primary"
            written = json.loads(layer.read_text(encoding="utf-8"))
            assert written["volume_uuid"] == deterministic_uuid("Alpha/Volume 3")
            primary = cbz.with_suffix(".mokuro")
            if primary.exists():  # this processor may have run it too
                assert json.loads(primary.read_text(encoding="utf-8"))["volume_uuid"] == (
                    written["volume_uuid"]
                )
            assert not (library["storage"] / ".ocr-failures.json").exists()
        finally:
            process.terminate()
            process.wait(timeout=30)

    def test_a_session_never_holds_more_than_two_volumes_at_once(
        self, tmp_path: Path, library: dict[str, Any], processor_config: Path
    ) -> None:
        """Spec section 3 rule 3, observed on the wire rather than asserted."""
        for volume in ("Volume 3", "Volume 4", "Volume 5"):
            _volume(library["storage"] / "library", "Alpha", volume)
        process = _spawn(
            processor_config, _script(tmp_path, pages=3, page_delay=0.05)
        )
        try:
            registry = library["registry"]
            assert _wait(lambda: _names(registry) == ["tower"])
            worker = library["worker"]
            seen: list[int] = []
            real_submit = worker._submit_session_volume

            def spy(*args: Any, **kwargs: Any) -> bool:
                result = real_submit(*args, **kwargs)
                session = args[0]
                seen.append(len(session.claims()))
                return result

            worker._submit_session_volume = spy  # type: ignore[method-assign]
            worker._scan_ocr_once()
            assert seen, "nothing was ever submitted"
            assert max(seen) <= 2
        finally:
            process.terminate()
            process.wait(timeout=30)

    def test_a_cancel_records_nothing_and_the_volume_is_pending_again(
        self, tmp_path: Path, library: dict[str, Any], processor_config: Path
    ) -> None:
        """A benchmark's pre-emption, driven through the real background loop.

        The scan that was running when `preempt_for_bench` returns is still
        alive, and so is the loop around it: while the queue is held, neither
        may claim a volume or open a session on the processor -- and once the
        hold is released, the work must simply resume.
        """
        process = _spawn(
            processor_config, _script(tmp_path, pages=3, page_delay=1.0, volume_delay=1.0)
        )
        worker = library["worker"]
        registry = library["registry"]
        failures = library["storage"] / ".ocr-failures.json"
        loop: threading.Thread | None = None
        try:
            assert _wait(lambda: _names(registry) == ["tower"])
            tower = next(e for e in registry.connected() if e.name == "tower")
            worker._running = True
            loop = threading.Thread(target=worker._run_ocr_loop, daemon=True)
            loop.start()
            assert _wait(lambda: bool(worker._inflight_ocr), timeout=45)

            # A benchmark FOR tower (spec section 3 rule 5): tower's session is
            # the one pre-empted.
            quiet, preempted = worker.preempt_for_bench(timeout=60.0, processor="tower")
            assert quiet is True
            assert preempted, "it named what it interrupted"
            assert worker._inflight_ocr == set()
            assert not failures.exists(), "a cancelled volume is not the volume's failure"
            pending = {(j["series"], j["volume"]) for j in worker.pending_jobs()}
            assert ("Alpha", "Volume 1") in pending

            # The killed session's bodies end on their own clock.
            assert _wait(lambda: tower.open_sessions == 0, timeout=10), (
                "the pre-empted session never closed on the processor"
            )
            # Held for longer than two supervisor ticks and two poll
            # intervals: nothing claimed, nothing opened, nothing recorded.
            held_until = time.monotonic() + 2.5
            while time.monotonic() < held_until:
                assert worker._inflight_ocr == set(), "a volume was claimed while held"
                assert tower.open_sessions == 0, "a session was opened while held"
                time.sleep(0.1)
            assert not failures.exists()
            assert worker._session_strikes == {}, "a kill we issued is not the row's crash"
            assert worker._stopped_generations == set()
            assert not list((library["storage"] / "library").rglob("*.hayai-ctd.mokuro"))

            worker.release_queue(processor="tower")
            assert _wait(
                lambda: sorted(
                    p.name
                    for p in (library["storage"] / "library").rglob("*.hayai-ctd.mokuro")
                ) == ["Volume 1.hayai-ctd.mokuro", "Volume 2.hayai-ctd.mokuro"],
                timeout=90,
            ), "the work never resumed after the hold was released"
            assert not failures.exists()
        finally:
            if worker.hardware_held("tower"):
                worker.release_queue(processor="tower")
            worker._running = False
            worker._stop_requested = True
            if loop is not None:
                loop.join(timeout=60)
            process.terminate()
            process.wait(timeout=30)

    def test_a_settings_change_mid_volume_records_nothing_and_re_offers_it(
        self, tmp_path: Path, library: dict[str, Any], processor_config: Path
    ) -> None:
        """`apply_settings` reaches a processor's session the way it reaches a
        local one: an output-affecting edit to the running row (its patch
        budget) kills the session, and that is the settings' doing -- not the
        volume's failure, not the row's strike -- and the volume is run again
        under the new recipe."""
        process = _spawn(
            processor_config, _script(tmp_path, pages=3, page_delay=1.0, volume_delay=1.0)
        )
        worker = library["worker"]
        registry = library["registry"]
        failures = library["storage"] / ".ocr-failures.json"
        try:
            assert _wait(lambda: _names(registry) == ["tower"])
            tower = next(e for e in registry.connected() if e.name == "tower")
            opened: list[dict[str, Any]] = []
            closed: list[str] = []
            real_send = tower.send

            def spy(op: Any) -> bool:
                if op.get("op") == "open_session":
                    opened.append(dict(op["generation"], sid=op["sid"]))
                elif op.get("op") == "close_session":
                    closed.append(str(op["sid"]))
                return bool(real_send(op))

            tower.send = spy  # type: ignore[method-assign]
            thread = threading.Thread(target=worker._scan_ocr_once, daemon=True)
            thread.start()
            assert _wait(lambda: bool(worker._inflight_ocr), timeout=45)
            assert opened and opened[0]["patch_budget"] == 512
            first = opened[0]["sid"]

            rows = [row.to_dict() for row in library["rows"]]
            rows[1]["patch_budget"] = 256
            worker.apply_settings(parse_generation_list(rows))
            # Well inside the time a volume needs to finish on its own (a
            # second's delay, then a second a page): this is the kill, and
            # what it handed back is offered again under the new recipe.
            assert _wait(lambda: first in closed, timeout=3), (
                "the session was never closed for the changed row"
            )
            assert _wait(lambda: len(opened) >= 2, timeout=3), (
                "nothing was re-offered to the processor"
            )
            assert opened[1]["patch_budget"] == 256
            assert not failures.exists(), "a settings change is not the volume's failure"
            assert worker._session_strikes == {}, "nor the row's"
            assert worker._stopped_generations == set()

            thread.join(timeout=90)
            assert not thread.is_alive()
            assert not failures.exists()
            assert worker._session_strikes == {}
            done = {
                p.stem.removesuffix(".hayai-ctd")
                for p in (library["storage"] / "library").rglob("*.hayai-ctd.mokuro")
            }
            pending = {j["volume"] for j in worker.pending_jobs(max_age=0.0)
                       if j["generation"] == "hayai-ctd"}
            assert {"Volume 1", "Volume 2"} <= done | pending, (
                "a cancelled volume is either re-run in the same scan or pending for the next"
            )

            worker._scan_ocr_once()
            assert sorted(
                p.name
                for p in (library["storage"] / "library").rglob("*.hayai-ctd.mokuro")
            ) == ["Volume 1.hayai-ctd.mokuro", "Volume 2.hayai-ctd.mokuro"]
            assert not failures.exists()
            assert len(opened) >= 2
            assert all(spec["patch_budget"] == 256 for spec in opened[1:]), (
                "every session after the edit runs the new recipe"
            )
        finally:
            worker._stop_requested = True
            process.terminate()
            process.wait(timeout=30)

    def test_an_events_body_arriving_after_the_library_ended_its_session_costs_nothing_else(
        self, tmp_path: Path, library: dict[str, Any], processor_config: Path
    ) -> None:
        """The race behind the settings-change test's flakiness, made certain.

        The first session's events body is held at the library until the
        library has killed that session (a settings change). It used to be
        answered 404 ``unknown`` -- the session was already forgotten -- and
        the processor, told to register again, tore down its registration
        and the session that had just replaced the killed one, and was gone
        for five seconds. Now it hears ``session_ended`` and carries on: the
        same registration, and the new session finishes both volumes.
        """
        from mokuro_bunko.ocr.remote.library_api import ProcessorAPI

        process = _spawn(
            processor_config, _script(tmp_path, pages=3, page_delay=0.3, volume_delay=0.3)
        )
        worker = library["worker"]
        registry = library["registry"]
        first: dict[str, str] = {}
        killed = threading.Event()
        held: list[str] = []
        real_events = ProcessorAPI._events

        def late_events(self: Any, environ: Any, start_response: Any, username: str,
                        processor_id: str, sid: str) -> Any:
            if sid == first.get("sid") and not killed.is_set():
                held.append(sid)
                killed.wait(timeout=20)
            return real_events(self, environ, start_response, username, processor_id, sid)

        try:
            assert _wait(lambda: _names(registry) == ["tower"])
            tower = next(e for e in registry.connected() if e.name == "tower")
            registered_as = tower.processor_id
            real_send = tower.send

            def spy(op: Any) -> bool:
                if op.get("op") == "open_session" and "sid" not in first:
                    first["sid"] = str(op["sid"])
                return bool(real_send(op))

            tower.send = spy  # type: ignore[method-assign]
            with patch.object(ProcessorAPI, "_events", late_events):
                thread = threading.Thread(target=worker._scan_ocr_once, daemon=True)
                thread.start()
                assert _wait(lambda: bool(held), timeout=45), "the first events body never came"
                rows = [row.to_dict() for row in library["rows"]]
                rows[1]["patch_budget"] = 256
                worker.apply_settings(parse_generation_list(rows))
                assert _wait(lambda: tower.was_ended(first["sid"]), timeout=5)
                killed.set()
                thread.join(timeout=90)
            assert not thread.is_alive()

            assert [e.processor_id for e in registry.connected() if not e.local] == [
                registered_as
            ], "the processor never registered again"
            assert sorted(
                p.name for p in (library["storage"] / "library").rglob("*.hayai-ctd.mokuro")
            ) == ["Volume 1.hayai-ctd.mokuro", "Volume 2.hayai-ctd.mokuro"], (
                "the new session finished both volumes in the same scan"
            )
            assert not (library["storage"] / ".ocr-failures.json").exists()
        finally:
            killed.set()
            worker._stop_requested = True
            process.terminate()
            process.wait(timeout=30)

    def test_a_disconnect_returns_the_claim_and_a_restart_picks_it_up(
        self, tmp_path: Path, library: dict[str, Any], processor_config: Path
    ) -> None:
        slow = _script(tmp_path, pages=3, page_delay=1.0, volume_delay=1.0)
        process = _spawn(processor_config, slow)
        worker = library["worker"]
        registry = library["registry"]
        try:
            assert _wait(lambda: _names(registry) == ["tower"])
            thread = threading.Thread(target=worker._scan_ocr_once, daemon=True)
            thread.start()
            assert _wait(lambda: bool(worker._inflight_ocr), timeout=45)
            process.terminate()
            process.wait(timeout=30)
            # Almost at once: the events body is the first channel to see
            # the processor go, and the library drops it there and then --
            # not up to a heartbeat later, when the stream next writes.
            assert _wait(lambda: not worker._inflight_ocr, timeout=10), (
                "the claim never came back"
            )
            assert not (library["storage"] / ".ocr-failures.json").exists(), (
                "a processor switched off mid-volume is not the volume's failure"
            )
            assert worker._session_strikes == {}, "nor the row's"
            assert worker._stopped_generations == set()
            hold = worker.processing_hold()
            assert hold is not None and hold["last"]["name"] == "tower"
        finally:
            worker._stop_requested = True

        again = _spawn(processor_config, _script(tmp_path, pages=3))
        try:
            assert _wait(lambda: _names(registry) == ["tower"], timeout=60)
            assert worker.processing_hold() is None
            worker._stop_requested = False
            worker._scan_ocr_once()
            assert sorted(
                p.name
                for p in (library["storage"] / "library").rglob("*.hayai-ctd.mokuro")
            ) == ["Volume 1.hayai-ctd.mokuro", "Volume 2.hayai-ctd.mokuro"]
        finally:
            again.terminate()
            again.wait(timeout=30)

    def test_a_processor_that_logs_in_mid_scan_joins_that_scan(
        self, tmp_path: Path, library: dict[str, Any], processor_config: Path
    ) -> None:
        """Load balancing is whoever is logged in, so a processor arriving
        while a backlog drains joins THAT scan -- the supervisor looks every
        SLOT_SUPERVISE_SECONDS -- rather than waiting for the next one, which
        behind a long backlog could be hours away."""
        for volume in ("Volume 3", "Volume 4", "Volume 5"):
            _volume(library["storage"] / "library", "Alpha", volume)
        slow = _script(tmp_path, pages=3, page_delay=1.0, volume_delay=1.0)
        second_state = tmp_path / "processor-2-state"
        second_state.mkdir()
        second_config = tmp_path / "processor-2.yaml"
        second_config.write_text(
            processor_config.read_text(encoding="utf-8")
            .replace("  name: tower\n", "  name: tower-2\n")
            .replace(str(tmp_path / "processor-state"), str(second_state)),
            encoding="utf-8",
        )
        worker = library["worker"]
        registry = library["registry"]
        first = _spawn(processor_config, slow)
        try:
            assert _wait(lambda: _names(registry) == ["tower"])
            thread = threading.Thread(target=worker._scan_ocr_once, daemon=True)
            thread.start()
            assert _wait(lambda: bool(worker._inflight_ocr), timeout=45)
            second = _spawn(second_config, slow)
            try:
                assert _wait(lambda: _names(registry) == ["tower", "tower-2"])
                arrived = time.monotonic()
                newcomer = next(e for e in registry.connected() if e.name == "tower-2")
                assert _wait(lambda: newcomer.open_sessions >= 1, timeout=10), (
                    "the newcomer was never offered a volume by the running scan"
                )
                assert time.monotonic() - arrived < 5.0, "it joined, but not promptly"
                assert thread.is_alive(), "and it was THAT scan it joined"
            finally:
                second.terminate()
                second.wait(timeout=30)
        finally:
            worker._stop_requested = True
            first.terminate()
            first.wait(timeout=30)

    def test_a_refused_login_exits_non_zero_and_is_listed(
        self, tmp_path: Path, library: dict[str, Any]
    ) -> None:
        path = tmp_path / "wrong.yaml"
        path.write_text(
            "library:\n"
            f"  url: http://127.0.0.1:{library['port']}\n"
            "  username: tower\n"
            "  password: definitely-the-wrong-one\n"
            "processor:\n"
            f"  storage: {tmp_path / 'wrong-state'}\n",
            encoding="utf-8",
        )
        process = _spawn(path, _script(tmp_path))
        out, _ = process.communicate(timeout=60)
        assert process.returncode == 1
        assert "refused" in out.lower()
        assert [f.username for f in library["registry"].failures()] == ["tower"], (
            "the admin panel has to be able to name the attempt"
        )


@pytest.mark.slow
class TestTheFinalFixWave:
    """The whole-branch review's findings, each against a real processor."""

    def test_a_new_processor_is_benchmarked_on_its_own_hardware_before_it_runs(
        self, tmp_path: Path, library: dict[str, Any], processor_config: Path
    ) -> None:
        """Spec sections 4 and 5: the pair is measured ON the processor (the
        sample pulled through its own route, the numbers stamped there), the
        best widths land in ITS profile, and only then does it run volumes --
        with those widths."""
        from mokuro_bunko.ocr.bench import BenchService, bench_path
        from mokuro_bunko.ocr.remote.profiles import ProcessorProfiles

        worker = library["worker"]
        registry = library["registry"]
        storage = library["storage"]
        row = library["rows"][1]
        worker.autobench = True
        worker.bench_service = BenchService(
            storage,
            worker=lambda: worker,
            generations=lambda: list(worker.generations),
            processors=registry.entries,
            profiles=ProcessorProfiles(storage),
        )
        process = _spawn(processor_config, _script(tmp_path, pages=3))
        try:
            assert _wait(lambda: _names(registry) == ["tower"])
            tower = next(e for e in registry.connected() if e.name == "tower")
            opened: list[dict[str, Any]] = []
            real_send = tower.send

            def spy(op: Any) -> bool:
                if op.get("op") in ("open_session", "bench"):
                    opened.append(dict(op))
                return bool(real_send(op))

            tower.send = spy  # type: ignore[method-assign]
            worker._scan_ocr_once()

            kinds = [op["op"] for op in opened]
            assert kinds and kinds[0] == "bench", f"measured first: {kinds}"
            profile = ProcessorProfiles(storage).row(
                "tower", row.id, recipe=row.output_affecting()
            )
            assert profile is not None and profile.bench is not None
            assert profile.bench["pages_per_second"] == pytest.approx(2.1)
            assert profile.pools["stage_workers"] == {"detect": 3}, "best applied"
            sessions = [op for op in opened if op["op"] == "open_session"]
            assert sessions and all(
                op["generation"]["pools"]["stage_workers"] == {"detect": 3}
                for op in sessions
            ), "and it ran with them"
            assert sorted(
                p.name for p in (storage / "library").rglob("*.hayai-ctd.mokuro")
            ) == ["Volume 1.hayai-ctd.mokuro", "Volume 2.hayai-ctd.mokuro"]
            assert not bench_path(storage).exists(), (
                "a processor's number never lands in this server's bench file"
            )
            assert not list((storage / ".processing").glob("bench-*.cbz")), (
                "the packed sample is removed"
            )
        finally:
            worker._stop_requested = True
            process.terminate()
            process.wait(timeout=30)

    def test_a_revoked_account_is_cut_off_and_the_processor_exits(
        self, tmp_path: Path, library: dict[str, Any], processor_config: Path,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        """I2: disabling a connected processor's account takes effect while
        it is connected -- its claims come back unrecorded, it registers
        again, is refused, and exits non-zero."""
        from mokuro_bunko.ocr.remote import library_api

        monkeypatch.setattr(library_api, "HEARTBEAT_SECONDS", 1.0)
        process = _spawn(
            processor_config, _script(tmp_path, pages=3, page_delay=1.0, volume_delay=1.0)
        )
        worker = library["worker"]
        registry = library["registry"]
        thread = threading.Thread(target=worker._scan_ocr_once, daemon=True)
        try:
            assert _wait(lambda: _names(registry) == ["tower"])
            thread.start()
            assert _wait(lambda: bool(worker._inflight_ocr), timeout=45)
            Database(library["storage"] / "mokuro.db").disable_user("tower")
            assert _wait(lambda: _names(registry) == [], timeout=15), (
                "the processor was never cut off"
            )
            assert _wait(lambda: not worker._inflight_ocr, timeout=15), (
                "its claims never came back"
            )
            assert process.wait(timeout=60) == 1, "a refused login ends the processor"
            output = process.stdout.read() if process.stdout else ""
            assert "Login refused" in output
            assert not (library["storage"] / ".ocr-failures.json").exists(), (
                "no volume paid for the revocation"
            )
            assert worker._session_strikes == {}
        finally:
            worker._stop_requested = True
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=30)
            thread.join(timeout=60)

    def test_a_frozen_processor_is_let_go_without_blaming_a_volume(
        self, tmp_path: Path, library: dict[str, Any], processor_config: Path
    ) -> None:
        """I3: a processor that stops answering while idle -- a suspended
        laptop, a pulled cable -- still looks connected: its stream only
        fails when a write finally does. The first session offered to it
        never opens its events body, and that is a disconnect: the claims
        come back unrecorded and no row is struck."""
        import signal

        process = _spawn(processor_config, _script(tmp_path, pages=3))
        worker = library["worker"]
        registry = library["registry"]
        try:
            assert _wait(lambda: _names(registry) == ["tower"])
            process.send_signal(signal.SIGSTOP)
            with patch("mokuro_bunko.ocr.watcher.EVENTS_OPEN_SECONDS", 3.0):
                started = time.monotonic()
                worker._scan_ocr_once()
                assert time.monotonic() - started < 60, "not the 600 s wedge"
            assert _names(registry) == []
            assert worker._inflight_ocr == set()
            assert not (library["storage"] / ".ocr-failures.json").exists()
            assert worker._session_strikes == {}
            assert worker._stopped_generations == set()
            pending = {(j["volume"], j["generation"]) for j in worker.pending_jobs(0.0)}
            assert ("Volume 1", "hayai-ctd") in pending
        finally:
            worker._stop_requested = True
            process.send_signal(signal.SIGCONT)
            process.terminate()
            process.wait(timeout=30)


def _primary(cbz: Path) -> None:
    """The volume's primary layer, so only the processor's row is owed."""
    cbz.with_suffix(".mokuro").write_text(
        json.dumps({"version": "0.2.5", "title": cbz.parent.name, "volume": cbz.stem,
                    "volume_uuid": f"uuid-{cbz.stem}", "pages": [], "chars": 0}),
        encoding="utf-8",
    )


def _ready(registry: Any) -> list[dict[str, float]]:
    entry = next(e for e in registry.entries() if not e.local)
    with entry.lock:
        return list(entry.transfer.ready)


def _runner_archive_lines(tmp_path: Path) -> list[str]:
    logs = sorted((tmp_path / "processor-state" / "logs").glob("session.*.log"))
    return [
        line
        for log in logs
        for line in log.read_text(encoding="utf-8").splitlines()
        if line.startswith("archive ") and "sha256=" in line
    ]


def _layer(storage: Path, stem: str) -> Path:
    return storage / "library" / "Alpha" / f"{stem}.hayai-ctd.mokuro"


@pytest.mark.slow
class TestTheArchiveOnTheWire:
    """Protocol 2's archive transfer, through the real library and a real
    `processor serve` process."""

    def test_a_runner_that_stalls_30_s_never_truncates_the_archive(
        self, tmp_path: Path, library: dict[str, Any], processor_config: Path
    ) -> None:
        """The root cause, end to end: a runner that reads nothing for three
        times the library's write timeout. The download is over long before
        the runner looks, in ONE request, and the bytes it then reads are the
        library's own."""
        import hashlib

        storage = library["storage"]
        for stem in ("Volume 1", "Volume 2"):
            (storage / "library" / "Alpha" / f"{stem}.cbz").unlink()
            (storage / "library" / "Alpha" / f"{stem}.mokuro").unlink()
        big = storage / "library" / "Alpha" / "Volume 9.cbz"
        data = _stored_cbz(big, [2 << 20] * 24)  # 48 MiB of stored pages
        _primary(big)
        process = _spawn(
            processor_config,
            _script(tmp_path, volume_delay=30, read_archive=True, stats=False),
        )
        try:
            registry = library["registry"]
            assert _wait(lambda: _names(registry) == ["tower"])
            library["worker"]._scan_ocr_once()
            assert _layer(storage, "Volume 9").is_file(), "the sidecar came home"
            assert not (storage / ".ocr-failures.json").exists()
            (ready,) = _ready(registry)
            assert ready["requests"] == 1
            assert ready["seconds"] < 10, "fetched while the runner slept"
            gets = library["faults"].gets("Volume 9.cbz")
            assert [g["status"] for g in gets] == [200]
            (line,) = _runner_archive_lines(tmp_path)
            assert f"sha256={hashlib.sha256(data).hexdigest()}" in line
            assert "stem=Volume 9 " in line
            written = json.loads(_layer(storage, "Volume 9").read_text(encoding="utf-8"))
            assert len(written["pages"]) == 24
        finally:
            process.terminate()
            process.wait(timeout=30)

    def test_a_download_cut_mid_body_resumes_against_the_real_library(
        self, tmp_path: Path, library: dict[str, Any], processor_config: Path
    ) -> None:
        storage = library["storage"]
        volume = storage / "library" / "Alpha" / "Volume 1.cbz"
        _stored_cbz(volume, [1 << 20] * 4)
        faults = library["faults"]
        faults.cut_first["Volume 1.cbz"] = 1 << 20
        process = _spawn(processor_config, _script(tmp_path, read_archive=True))
        try:
            registry = library["registry"]
            assert _wait(lambda: _names(registry) == ["tower"])
            library["worker"]._scan_ocr_once()
            first, second = faults.gets("Volume 1.cbz")
            assert first["status"] == 200 and first["range"] is None
            assert second["range"] == f"bytes={1 << 20}-"
            assert second["if_range"] == first["etag"]
            assert second["status"] == 206, "real wsgidav resumed it"
            assert _layer(storage, "Volume 1").is_file()
            assert _layer(storage, "Volume 2").is_file()
            assert not (storage / ".ocr-failures.json").exists()
            assert sorted(r["requests"] for r in _ready(registry)) == [1.0, 2.0]
        finally:
            process.terminate()
            process.wait(timeout=30)

    def test_an_archive_replaced_mid_download_is_fetched_again_whole(
        self, tmp_path: Path, library: dict[str, Any], processor_config: Path
    ) -> None:
        """Resume, never splice: the stale If-Range gets the whole new file."""
        storage = library["storage"]
        volume = storage / "library" / "Alpha" / "Volume 1.cbz"
        old = _stored_cbz(volume, [1 << 20] * 4)
        # Five pages in the same number of bytes: a stored member costs 100
        # bytes of headers with these names.
        scratch = tmp_path / "replacement.cbz"
        new = _stored_cbz(scratch, [838_860] * 4 + [(4 << 20) - 100 - 838_860 * 4])
        assert len(new) == len(old)
        faults = library["faults"]
        faults.replace_first["Volume 1.cbz"] = (volume, new)
        faults.cut_first["Volume 1.cbz"] = 1 << 20
        process = _spawn(processor_config, _script(tmp_path, read_archive=True))
        try:
            registry = library["registry"]
            assert _wait(lambda: _names(registry) == ["tower"])
            library["worker"]._scan_ocr_once()
            first, second = faults.gets("Volume 1.cbz")
            assert second["if_range"] == first["etag"]
            assert second["status"] == 200, "a stale If-Range is answered with the whole file"
            written = json.loads(_layer(storage, "Volume 1").read_text(encoding="utf-8"))
            assert len(written["pages"]) == 5, "the NEW archive's pages"
            assert not (storage / ".ocr-failures.json").exists()
            assert sorted(r["restarts"] for r in _ready(registry)) == [0.0, 1.0]
        finally:
            process.terminate()
            process.wait(timeout=30)

    @pytest.mark.skipif(
        sys.platform == "win32" or (hasattr(os, "geteuid") and os.geteuid() == 0),
        reason="root reads a mode-000 file",
    )
    def test_a_file_the_library_cannot_read_is_recorded(
        self, tmp_path: Path, library: dict[str, Any], processor_config: Path
    ) -> None:
        """wsgidav answers 500 for a file it cannot open; the processor gives
        the claim back as `stalled` (500 twice), and the library's OWN read
        of the file is what records it -- the breaker never moves."""
        storage = library["storage"]
        volume = storage / "library" / "Alpha" / "Volume 1.cbz"
        volume.chmod(0)
        process = _spawn(processor_config, _script(tmp_path, read_archive=True))
        try:
            registry = library["registry"]
            assert _wait(lambda: _names(registry) == ["tower"])
            worker = library["worker"]
            worker._scan_ocr_once()
            failures = json.loads((storage / ".ocr-failures.json").read_text(encoding="utf-8"))
            record = failures["Alpha/Volume 1.cbz@hayai-ctd"]
            assert record["error"].startswith(
                "the library cannot read its own copy of this archive"
            )
            assert "Permission denied" in record["error"]
            assert [g["status"] for g in library["faults"].gets("Volume 1.cbz")] == [500, 500]
            assert all(b.consecutive == 0 for b in worker._breakers.values())
            assert worker._session_strikes == {}
            assert _layer(storage, "Volume 2").is_file()
        finally:
            volume.chmod(0o644)
            process.terminate()
            process.wait(timeout=30)
