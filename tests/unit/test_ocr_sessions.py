"""Persistent model sessions: one runner open, volumes streaming through it.

The rules these tests pin down, all against the fake runner in
``tests/fixtures/fake_runner.py`` (the real one needs a GPU, the engines
environment and several gigabytes of weights, and none of that is what the
SERVER side has to get right):

* one session per generation, volumes submitted in queue order, a LOOKAHEAD
  of two held open;
* two volumes overlap in one process with their own progress cards and their
  own finishes;
* a finished volume's sidecar is collected into the library and its `stats`
  become a congestion entry;
* one volume failing is one volume's failure;
* a runner that dies blames the OLDEST unfinished volume and returns the
  rest untouched; two such deaths stop that row for the scan;
* a runner that goes silent is killed on the wedge timeout;
* ROW-PRIORITY pre-emption happens at a volume boundary and never kills;
* a settings change kills the session whose recipe changed, and a rename or
  a pools edit does not;
* a session closes when its row is drained, and a volume that arrives first
  keeps it open;
* monolithic rows keep the per-volume path and interleave in row order;
* shutdown leaves no child behind;
* a BENCHMARK's pre-emption (`OCRWorker.preempt_for_bench`) is a different
  thing from row-priority pre-emption above: it ends whatever OCR is
  running -- a session or a per-volume job, whatever row it belongs to --
  at once, through the exact cancel-without-failure path `apply_settings`
  uses, so the numbers a benchmark measures are never taken beside a job the
  queue just started.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
import time
import zipfile
from collections.abc import Sequence
from pathlib import Path
from typing import Any
from unittest.mock import patch

import pytest

from mokuro_bunko.ocr.congestion import CongestionHistory
from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.session import OcrSession, SessionVolume
from mokuro_bunko.ocr.watcher import OCRWorker

FAKE_RUNNER = Path(__file__).resolve().parents[1] / "fixtures" / "fake_runner.py"

PRIMARY: dict[str, Any] = {"name": "mokuro", "engine": "mokuro", "primary": True}
HAYAI: dict[str, Any] = {"name": "hayai-nova", "engine": "hayai-nova"}
PADDLE: dict[str, Any] = {"name": "paddle-manga", "engine": "paddle-manga"}


def _gens(*rows: dict[str, Any]) -> list[GenerationSpec]:
    return parse_generation_list([dict(row) for row in rows])


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    (tmp_path / "library").mkdir()
    (tmp_path / "inbox").mkdir()
    return tmp_path


def _make_cbz(path: Path, pages: int = 3) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w") as zf:
        for n in range(pages):
            zf.writestr(f"page_{n:03d}.jpg", b"fake image data")
    return path


def _library(storage: Path, *, primary_done: bool = True, **series: list[str]) -> None:
    """Volumes, with their primary `<Volume>.mokuro` already written.

    The default, because a volume still owing the primary row its file
    offers ONLY that row, and every session test here is about a SECONDARY
    row streaming volumes.
    """
    for name, volumes in series.items():
        for volume in volumes:
            cbz = _make_cbz(storage / "library" / name / f"{volume}.cbz")
            if primary_done:
                cbz.with_suffix(".mokuro").write_text(
                    json.dumps({"version": "0.0", "volume_uuid": f"uuid-{name}-{volume}",
                                "pages": [], "chars": 0}),
                    encoding="utf-8",
                )


def _script(storage: Path, **rules: Any) -> Path:
    path = storage / "fake-runner-script.json"
    path.write_text(json.dumps(rules), encoding="utf-8")
    return path


def _worker(
    storage: Path,
    generations: Sequence[GenerationSpec],
    *,
    concurrency: int = 1,
    script: Path | None = None,
    status: list[str] | None = None,
) -> OCRWorker:
    """A worker whose composed rows run the FAKE runner as their session."""
    worker = OCRWorker(
        storage_path=storage,
        poll_interval=30.0,
        generations=list(generations),
        engines_python_path=Path(sys.executable),
        concurrency=concurrency,
        sessions=True,
        status_callback=(status.append if status is not None else None),
    )
    for slot in worker._slots:
        _redirect_to_fake(slot.processor, script)
    return worker


def _redirect_to_fake(processor: Any, script: Path | None) -> None:
    """Point this processor's sessions at the fake runner script."""
    real_command = processor.session_command
    real_env = processor.ocr_env

    def session_command(generation: GenerationSpec, session_log: Path) -> list[str]:
        command = real_command(generation, session_log)
        command[1] = str(FAKE_RUNNER)
        return command

    def ocr_env(generation: GenerationSpec | None = None) -> dict[str, str]:
        env = real_env(generation)
        if script is not None:
            env["FAKE_RUNNER_SCRIPT"] = str(script)
        return env

    processor.session_command = session_command  # type: ignore[method-assign]
    processor.ocr_env = ocr_env  # type: ignore[method-assign]


def _sidecars(storage: Path, suffix: str) -> list[str]:
    return sorted(p.name for p in (storage / "library").rglob(f"*{suffix}"))


def _failures(storage: Path) -> dict[str, Any]:
    path = storage / ".ocr-failures.json"
    if not path.exists():
        return {}
    return json.loads(path.read_text(encoding="utf-8"))


class TestTheProtocol:
    """The exact bytes the server writes and the events it reads."""

    def test_the_volume_op_carries_the_archive_and_every_path(self, tmp_path: Path) -> None:
        volume = SessionVolume(
            id="v1",
            workspace=tmp_path / "ws",
            output=tmp_path / "ws" / "Vol 1.hayai-nova.mokuro",
            cache_dir=tmp_path / "ws" / "_ocr" / "g-2" / "Vol 1",
            detect_dir=tmp_path / "ws" / "_detect" / "g-2",
            log=tmp_path / "logs" / "Alpha_Vol 1.hayai-nova.log",
            title="Alpha",
            volume="Vol 1",
            archive=tmp_path / "library" / "Alpha" / "Vol 1.cbz",
            title_uuid="title-uuid",
            volume_uuid="volume-uuid",
        )
        op = volume.to_op()
        assert op == {
            "op": "volume",
            "id": "v1",
            "workspace": str(tmp_path / "ws"),
            "output": str(tmp_path / "ws" / "Vol 1.hayai-nova.mokuro"),
            "cache_dir": str(tmp_path / "ws" / "_ocr" / "g-2" / "Vol 1"),
            "detect_dir": str(tmp_path / "ws" / "_detect" / "g-2"),
            "log": str(tmp_path / "logs" / "Alpha_Vol 1.hayai-nova.log"),
            "title": "Alpha",
            "volume": "Vol 1",
            "title_uuid": "title-uuid",
            "volume_uuid": "volume-uuid",
            "archive": str(tmp_path / "library" / "Alpha" / "Vol 1.cbz"),
        }
        # An already-extracted directory is the other way in (the CLI, the
        # tests and the benchmark sample), and then there is no archive key.
        directory = SessionVolume(
            id="v2",
            workspace=tmp_path / "ws",
            output=tmp_path / "out.mokuro",
            cache_dir=tmp_path / "cache",
            detect_dir=tmp_path / "detect",
            log=tmp_path / "log",
            title="Alpha",
            volume="Vol 2",
            input_dir=tmp_path / "pages",
        ).to_op()
        assert "archive" not in directory
        assert directory["input"] == str(tmp_path / "pages")

    def test_the_session_command_is_the_static_half_only(self, storage: Path) -> None:
        """``--serve`` fixes what the session IS; volumes arrive as ops."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        rows = _gens(PRIMARY, {"name": "hn", "engine": "hayai-nova", "detector": "ctd",
                               "patch_budget": 256,
                               "pools": {"stage_workers": {"detect": 3}}})
        proc = OCRProcessor(
            storage_path=storage, generations=rows, engines_python_path=Path("/usr/bin/python3")
        )
        cmd = proc.session_command(rows[1], storage / "session.log")
        assert cmd[0] == "/usr/bin/python3"
        assert Path(cmd[1]).name == "engine_runner.py"
        assert cmd[2] == "--serve"
        args = dict(zip(cmd[3::2], cmd[4::2], strict=True))
        assert args["--engine"] == "hayai-nova"
        assert args["--detector"] == "ctd"
        assert args["--patches"] == "256"
        assert args["--stage-workers"] == "detect=3"
        assert args["--session-log"] == str(storage / "session.log")
        assert args["--generator"].startswith("mokuro-bunko ")
        # Nothing per volume: those are ops, because a session outlives them.
        for flag in ("--input", "--output", "--cache-dir", "--volume-uuid"):
            assert flag not in cmd

    def test_the_session_command_carries_the_device_choice(self, storage: Path) -> None:
        """Where the models go is part of what the session IS (Addendum 7)."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        rows = _gens(PRIMARY, {"name": "hn", "engine": "hayai-nova", "detector": "ctd",
                               "pools": {"stage_device": {"detect": "cpu",
                                                          "engine": "gpu:0"}}})  # fmt: skip
        proc = OCRProcessor(
            storage_path=storage, generations=rows, engines_python_path=Path("/usr/bin/python3")
        )
        cmd = proc.session_command(rows[1], storage / "session.log")
        args = dict(zip(cmd[3::2], cmd[4::2], strict=True))
        assert args["--stage-device"] == "detect=cpu,engine=gpu:0"
        # An untuned row says nothing and the runner probes as it always did.
        plain = _gens(PRIMARY, {"name": "hn2", "engine": "hayai-nova", "detector": "ctd"})
        proc2 = OCRProcessor(
            storage_path=storage, generations=plain, engines_python_path=Path("/usr/bin/python3")
        )
        assert "--stage-device" not in proc2.session_command(plain[1], storage / "s2.log")

    def test_a_monolithic_rows_device_is_the_mokuro_command_line(self, storage: Path) -> None:
        """Its one stage is the fork's pipeline: a flag and an environment."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        on_cpu = _gens({**PRIMARY, "pools": {"stage_workers": {"mokuro": 6},
                                             "stage_device": {"mokuro": "cpu"}}})[0]  # fmt: skip
        flags, env = OCRProcessor._mokuro_placement(on_cpu)
        assert flags == ["--force_cpu", "--num_workers", "6"]
        assert env == {}

        second_card = _gens({**PRIMARY, "pools": {"stage_device": {"mokuro": "gpu:1"}}})[0]
        flags, env = OCRProcessor._mokuro_placement(second_card)
        # mokuro has no device index of its own: the card is chosen by hiding
        # the others, in that subprocess only.
        assert flags == []
        assert env == {"CUDA_VISIBLE_DEVICES": "1", "HIP_VISIBLE_DEVICES": "1"}

        auto = _gens(PRIMARY)[0]
        assert OCRProcessor._mokuro_placement(auto) == ([], {})

    def test_the_cli_fallback_and_the_serve_process_place_a_device_alike(self) -> None:
        """ONE source of truth for "device -> mokuro flags/env", two callers.

        The ``mokuro`` stage is the fork's page pipeline whether it is reached
        by a command line (a package with no serve module) or by a pipe
        (ADDENDUM 8), so a row's Device select must mean the same thing on
        both paths -- which is only true while both ask the same function.
        """
        from mokuro_bunko.ocr.engine_runner import mokuro_placement
        from mokuro_bunko.ocr.processor import OCRProcessor

        for device in ("cpu", "gpu:1", "auto", ""):
            pools = {"stage_device": {"mokuro": device}} if device else {}
            row = _gens({**PRIMARY, "pools": pools})[0]
            flags, env = OCRProcessor._mokuro_placement(row)
            placement = mokuro_placement(device)
            # The CLI adds --num_workers on top; the placement half is the
            # runner's, verbatim.
            assert flags == placement.flags
            assert env == placement.env

    def test_a_served_rows_device_reaches_the_runner_as_a_stage_device(
        self, storage: Path
    ) -> None:
        """``mokuro`` is a stage of the served ROAD, so the flag carries it.

        The runner is what spawns the serve process here, and it is the runner
        that turns the id into ``--force_cpu`` or a VISIBLE_DEVICES pair.
        """
        from mokuro_bunko.ocr.processor import OCRProcessor

        pools = {"stage_device": {"mokuro": "gpu:1"}, "stage_workers": {"mokuro": 6}}
        rows = _gens({**PRIMARY, "pools": pools})
        proc = OCRProcessor(
            storage_path=storage, generations=rows, engines_python_path=Path("/usr/bin/python3")
        )
        cmd = proc.session_command(rows[0], storage / "session.log")
        assert cmd[cmd.index("--stage-device") + 1] == "mokuro=gpu:1"
        assert cmd[cmd.index("--stage-workers") + 1] == "mokuro=6"
        # ... along with the interpreter the serve process needs.
        assert "--mokuro-python" in cmd

    def test_garbage_on_stdout_is_counted_and_survived(self, storage: Path) -> None:
        rows = _gens(PRIMARY, HAYAI)
        script = _script(storage, garbage=["loading weights...", "{not json"], pages=1)
        session = _fake_session(storage, rows[1], script)
        assert session.start()
        events = _drain(session)
        assert [e["event"] for e in events if e["event"] != "exit"] == ["ready"]
        assert session.garbage_lines == 2

    def test_a_runner_that_cannot_start_reports_spawn_failed_then_exit(
        self, storage: Path
    ) -> None:
        rows = _gens(PRIMARY, HAYAI)
        session = OcrSession(
            rows[1],
            ["/definitely/not/a/binary", "--serve"],
            session_log=storage / "session.log",
        )
        assert session.start() is False
        assert session.poll_event(timeout=1.0)["event"] == "spawn_failed"
        assert session.poll_event(timeout=1.0)["event"] == "exit"


def _fake_session(storage: Path, row: GenerationSpec, script: Path | None) -> OcrSession:
    env = dict(os.environ)
    env["PYTHONUNBUFFERED"] = "1"
    if script is not None:
        env["FAKE_RUNNER_SCRIPT"] = str(script)
    return OcrSession(
        row,
        [sys.executable, str(FAKE_RUNNER), "--serve", "--session-log", str(storage / "s.log")],
        session_log=storage / "s.log",
        env=env,
    )


def _drain(session: OcrSession, timeout: float = 10.0) -> list[dict[str, Any]]:
    session.close()
    events: list[dict[str, Any]] = []
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        event = session.poll_event(timeout=0.5)
        if event is None:
            continue
        events.append(event)
        if event["event"] == "exit":
            break
    return events


class TestStreamingVolumes:
    def test_one_session_takes_every_volume_of_its_row_in_queue_order(
        self, storage: Path
    ) -> None:
        """One runner, one `ready`, every volume of the row, then a clean close."""
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1", "Volume 2"], Beta=["Volume 1"])
        script = _script(storage, pages=2)
        worker = _worker(storage, rows, script=script)
        opened: list[Any] = []
        real_open = worker.processor.open_session

        def spy(generation: GenerationSpec, log: Path) -> Any:
            session = real_open(generation, log)
            opened.append(session)
            return session

        worker.processor.open_session = spy  # type: ignore[method-assign]
        worker._scan_ocr_once()

        assert len(opened) == 1, "one runner for the whole row, not one per volume"
        assert _sidecars(storage, ".hayai-nova.mokuro") == [
            "Volume 1.hayai-nova.mokuro",
            "Volume 1.hayai-nova.mokuro",
            "Volume 2.hayai-nova.mokuro",
        ]
        assert _failures(storage) == {}

    def test_the_lookahead_holds_two_volumes_at_once(self, storage: Path) -> None:
        """Two submitted-but-unfinished volumes, each with its own progress card.

        The feeder must always have the NEXT archive to roll into, and the
        Queue page must show both -- a session with several volumes in one
        process is exactly where a single-job progress file would lie.
        """
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1", "Volume 2", "Volume 3"])
        script = _script(storage, pages=6, page_delay=0.05)
        worker = _worker(storage, rows, script=script)
        seen: list[int] = []
        real_set = worker._set_active_progress

        def spy(job: tuple[Path, str], data: dict[str, Any]) -> None:
            real_set(job, data)
            with worker._lock:
                seen.append(len(worker._active_progress))

        worker._set_active_progress = spy  # type: ignore[method-assign]
        worker._scan_ocr_once()

        assert max(seen) == 2, "two volumes in flight, never three"
        assert len(_sidecars(storage, ".hayai-nova.mokuro")) == 3
        # And every card is closed when the session ends.
        assert worker._active_progress == {}

    def test_a_finished_volume_appends_its_congestion_entry(self, storage: Path) -> None:
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1"])
        worker = _worker(storage, rows, script=_script(storage, pages=12))
        worker._scan_ocr_once()

        history = CongestionHistory(storage).load()
        runs = history[rows[1].id]
        assert len(runs) == 1
        assert runs[0]["volume"] == str(Path("Alpha") / "Volume 1.cbz")
        assert runs[0]["pages"] == 12
        assert [stage["key"] for stage in runs[0]["stages"]] == ["detect", "engine"]
        assert runs[0]["queues"][0]["name"] == "detect->engine"

    def test_the_collected_sidecar_is_normalized_and_inherits_the_volume_uuid(
        self, storage: Path
    ) -> None:
        """A secondary layer is stamped and carries the volume's own uuid."""
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1"])
        worker = _worker(storage, rows, script=_script(storage, pages=1))
        worker._scan_ocr_once()

        written = json.loads(
            (storage / "library" / "Alpha" / "Volume 1.hayai-nova.mokuro").read_text(
                encoding="utf-8"
            )
        )
        assert written["volume_uuid"] == "uuid-Alpha-Volume 1"
        assert written["title"] == "Alpha"
        assert written["volume"] == "Volume 1"
        assert written["ocr_engine"]["generation"] == "hayai-nova"

    def test_a_failed_volume_is_one_volume_s_failure(self, storage: Path) -> None:
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1", "Volume 2"])
        script = _script(storage, pages=2, volumes={"v1": {"fail": "every page failed"}})
        worker = _worker(storage, rows, script=script)
        worker._scan_ocr_once()

        failures = _failures(storage)
        assert list(failures) == ["Alpha/Volume 1.cbz@hayai-nova"]
        assert failures["Alpha/Volume 1.cbz@hayai-nova"]["error"] == "every page failed"
        # The session carried on: the other volume is finished.
        assert _sidecars(storage, ".hayai-nova.mokuro") == ["Volume 2.hayai-nova.mokuro"]

    def test_a_volume_whose_sidecar_never_appeared_is_a_failure_not_a_silent_pass(
        self, storage: Path
    ) -> None:
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1"])
        script = _script(storage, pages=1, volumes={"v1": {"no_sidecar": True}})
        worker = _worker(storage, rows, script=script)
        worker._scan_ocr_once()

        failures = _failures(storage)
        assert "no valid hayai-nova sidecar generated" in (
            failures["Alpha/Volume 1.cbz@hayai-nova"]["error"]
        )

    def test_the_workspace_is_dropped_when_a_volume_ends(self, storage: Path) -> None:
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1"])
        worker = _worker(storage, rows, script=_script(storage, pages=1))
        worker._scan_ocr_once()

        leftovers = [
            p
            for p in (storage / ".processing").iterdir()
            if p.is_dir() and not p.name.startswith("runner-")
        ]
        assert leftovers == []


class TestCrashes:
    def test_a_crash_blames_the_oldest_and_returns_the_rest(self, storage: Path) -> None:
        """One death, one failure record; the lookahead is not punished for it."""
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1", "Volume 2", "Volume 3"])
        script = _script(
            storage,
            pages=4,
            page_delay=0.05,
            volumes={"v1": {"die_after_pages": 1}},
        )
        worker = _worker(storage, rows, script=script)
        worker._scan_ocr_once()

        failures = _failures(storage)
        assert list(failures) == ["Alpha/Volume 1.cbz@hayai-nova"]
        assert "exited" in failures["Alpha/Volume 1.cbz@hayai-nova"]["error"]
        # Nothing is left claimed: the released volumes are free again.
        assert worker._inflight_ocr == set()

    def test_two_deaths_in_a_row_stop_the_generation_for_the_scan(
        self, storage: Path
    ) -> None:
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=[f"Volume {n}" for n in range(1, 7)])
        # Every volume kills the runner mid-page, so no session ever
        # completes one.
        script = _script(
            storage,
            pages=4,
            volumes={f"v{n}": {"die_after_pages": 0} for n in range(1, 20)},
        )
        worker = _worker(storage, rows, script=script)
        opened: list[Any] = []
        real_open = worker.processor.open_session

        def spy(generation: GenerationSpec, log: Path) -> Any:
            session = real_open(generation, log)
            opened.append(session)
            return session

        worker.processor.open_session = spy  # type: ignore[method-assign]
        worker._scan_ocr_once()

        assert len(opened) == 2, "given up on after two deaths, not retried per volume"
        assert (rows[1].id, "local") in worker._stopped_generations
        # And the next scan is a fresh chance.
        worker._scan_ocr_once()
        assert len(opened) == 4

    def test_a_wedged_runner_is_killed_on_the_timeout(self, storage: Path) -> None:
        """No event for the wedge window means the pipeline stopped, not slowed."""
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1"])
        script = _script(storage, pages=2, wedge_after=0)
        worker = _worker(storage, rows, script=script)
        with patch("mokuro_bunko.ocr.watcher.SESSION_WEDGE_SECONDS", 1.0):
            worker._scan_ocr_once()

        failures = _failures(storage)
        assert "stopped responding" in failures["Alpha/Volume 1.cbz@hayai-nova"]["error"]
        assert worker._inflight_ocr == set()

    def test_an_unclosed_exit_after_a_finished_volume_does_not_strike_the_row(
        self, storage: Path
    ) -> None:
        """A session that DID finish a volume gets its strike counter reset."""
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1", "Volume 2", "Volume 3", "Volume 4"])
        script = _script(storage, pages=1, exit_unclosed_after=1)
        worker = _worker(storage, rows, script=script)
        worker._scan_ocr_once()

        assert (rows[1].id, "local") not in worker._stopped_generations
        # Each session finished exactly one volume before exiting, and the
        # next session picked the queue up again.
        assert len(_sidecars(storage, ".hayai-nova.mokuro")) >= 2


class TestPreemptionAndClosing:
    def test_an_earlier_row_pre_empts_at_a_volume_boundary(self, storage: Path) -> None:
        """Row order is priority: a new upload's primary sidecar goes first.

        Never by killing: the volumes already accepted finish, the session
        closes, and the slot switches.
        """
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1", "Volume 2", "Volume 3"])
        script = _script(storage, pages=4, page_delay=0.05)
        worker = _worker(storage, rows, script=script)

        ran: list[tuple[str, str]] = []
        killed: list[bool] = []

        def fake_mokuro(cbz: Path, generation: GenerationSpec) -> bool:
            ran.append((cbz.stem, generation.name))
            cbz.with_suffix(".mokuro").write_text(
                json.dumps({"version": "0.0", "pages": [], "chars": 0}), encoding="utf-8"
            )
            return True

        # A new volume lands as soon as the first hayai volume finishes.
        real_install = worker.processor.install_session_sidecar
        dropped = threading.Event()

        def install(cbz: Path, generation: GenerationSpec, sidecar: Path) -> str | None:
            result = real_install(cbz, generation, sidecar)
            if not dropped.is_set():
                dropped.set()
                _make_cbz(storage / "library" / "Gamma" / "Volume 1.cbz")
            return result

        worker.processor.install_session_sidecar = install  # type: ignore[method-assign]
        with patch.object(
            worker.processor, "process_library_ocr", side_effect=fake_mokuro
        ), patch.object(worker._slots[0].processor, "cancel_active",
                        side_effect=lambda: killed.append(True) or False):
            worker._scan_ocr_once()

        assert ("Volume 1", "mokuro") in ran, "the new upload got its primary sidecar"
        assert killed == [], "pre-emption never kills a session"
        # And the backlog was picked up again afterwards -- the three volumes
        # it started with plus the new one, which became an ordinary
        # candidate for the second row once it had its primary file.
        assert len(_sidecars(storage, ".hayai-nova.mokuro")) == 4

    def test_a_volume_that_arrives_while_the_last_one_finishes_keeps_it_open(
        self, storage: Path
    ) -> None:
        """The top-up claim before closing IS the re-check."""
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1"])
        script = _script(storage, pages=2)
        worker = _worker(storage, rows, script=script)

        opened: list[Any] = []
        real_open = worker.processor.open_session

        def spy(generation: GenerationSpec, log: Path) -> Any:
            session = real_open(generation, log)
            opened.append(session)
            return session

        worker.processor.open_session = spy  # type: ignore[method-assign]
        real_install = worker.processor.install_session_sidecar
        added = threading.Event()

        def install(cbz: Path, generation: GenerationSpec, sidecar: Path) -> str | None:
            result = real_install(cbz, generation, sidecar)
            if not added.is_set():
                added.set()
                late = _make_cbz(storage / "library" / "Alpha" / "Volume 2.cbz")
                late.with_suffix(".mokuro").write_text(
                    json.dumps({"version": "0.0", "volume_uuid": "late", "pages": [],
                                "chars": 0}),
                    encoding="utf-8",
                )
            return result

        worker.processor.install_session_sidecar = install  # type: ignore[method-assign]
        worker._scan_ocr_once()

        assert len(opened) == 1, "the late volume rode the session that was still open"
        assert len(_sidecars(storage, ".hayai-nova.mokuro")) == 2


class TestSettingsChanges:
    def _running_session(
        self, storage: Path, rows: list[GenerationSpec]
    ) -> tuple[OCRWorker, threading.Thread, threading.Event]:
        _library(storage, Alpha=["Volume 1", "Volume 2"])
        script = _script(storage, pages=200, page_delay=0.02)
        worker = _worker(storage, rows, script=script)
        running = threading.Event()
        real_submit = worker._submit_session_volume

        def spy(*args: Any, **kwargs: Any) -> bool:
            result = real_submit(*args, **kwargs)
            running.set()
            return result

        worker._submit_session_volume = spy  # type: ignore[method-assign]
        thread = threading.Thread(target=worker._scan_ocr_once, daemon=True)
        thread.start()
        assert running.wait(timeout=20)
        time.sleep(0.5)
        return worker, thread, running

    def test_a_removed_row_kills_its_session_and_records_nothing(
        self, storage: Path
    ) -> None:
        rows = _gens(PRIMARY, HAYAI)
        worker, thread, _ = self._running_session(storage, rows)
        try:
            worker.apply_settings(_gens(PRIMARY))
        finally:
            worker._stop_requested = True
            thread.join(timeout=30)
        assert _failures(storage) == {}, "a cancelled volume is not the volume's failure"
        assert worker._inflight_ocr == set()

    @pytest.mark.parametrize(
        "edit,kills",
        [
            ({"name": "hayai-nova", "engine": "hayai-nova", "detector": "ctd"}, True),
            ({"name": "renamed-row", "engine": "hayai-nova"}, False),
            (
                {"name": "hayai-nova", "engine": "hayai-nova",
                 "pools": {"stage_workers": {"detect": 4}}},
                False,
            ),
        ],
        ids=["detector-changed", "renamed", "pools"],
    )
    def test_only_an_output_affecting_edit_kills_the_session(
        self, storage: Path, edit: dict[str, Any], kills: bool
    ) -> None:
        """A rename applies to the NEXT volume; pools apply to the next session."""
        rows = _gens(PRIMARY, HAYAI)
        worker, thread, _ = self._running_session(storage, rows)
        session = worker._slots[0].session
        assert session is not None
        try:
            worker.apply_settings(_gens(PRIMARY, {**edit, "id": rows[1].id}))
            time.sleep(0.5)
            assert (not session.is_alive()) is kills
        finally:
            worker._stop_requested = True
            if session is not None:
                session.kill()
            thread.join(timeout=30)


class TestMonolithicRowsAreTheException:
    def test_a_mokuro_row_never_opens_a_session(self, storage: Path) -> None:
        rows = _gens(PRIMARY)
        _library(storage, Alpha=["Volume 1"], primary_done=False)
        worker = _worker(storage, rows)
        opened: list[Any] = []
        worker.processor.open_session = lambda *a, **k: opened.append(a)  # type: ignore
        with patch.object(
            worker.processor, "process_library_ocr", return_value=True
        ) as per_volume:
            worker._scan_ocr_once()
        assert opened == []
        assert per_volume.call_count == 1

    def test_both_kinds_run_in_one_queue_in_row_order(self, storage: Path) -> None:
        """A monolithic primary and a composed secondary, interleaved correctly."""
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1", "Volume 2"], primary_done=False)
        worker = _worker(storage, rows, script=_script(storage, pages=1))
        order: list[tuple[str, str]] = []

        def fake_mokuro(cbz: Path, generation: GenerationSpec) -> bool:
            order.append((cbz.stem, generation.name))
            cbz.with_suffix(".mokuro").write_text(
                json.dumps({"version": "0.0", "volume_uuid": f"u-{cbz.stem}", "pages": [],
                            "chars": 0}),
                encoding="utf-8",
            )
            return True

        real_install = worker.processor.install_session_sidecar

        def install(cbz: Path, generation: GenerationSpec, sidecar: Path) -> str | None:
            order.append((cbz.stem, generation.name))
            return real_install(cbz, generation, sidecar)

        worker.processor.install_session_sidecar = install  # type: ignore[method-assign]
        with patch.object(worker.processor, "process_library_ocr", side_effect=fake_mokuro):
            worker._scan_ocr_once()

        # Every volume gets the first row's file before the second row runs.
        assert [name for _, name in order] == [
            "mokuro",
            "mokuro",
            "hayai-nova",
            "hayai-nova",
        ]


class _HeldSession:
    """Just enough of an open session for the device preference to read it."""

    def __init__(self, generation: GenerationSpec) -> None:
        self.generation = generation


class TestWhichRowASlotPicks:
    """Addendum 7: with two cards, two rows should run on two cards."""

    @staticmethod
    def _pinned(storage: Path, first: str, second: str) -> Any:
        rows = _gens(
            PRIMARY,
            {**HAYAI, "pools": {"stage_device": {"engine": first}}},
            {**PADDLE, "pools": {"stage_device": {"engine": second}}},
        )
        _library(storage, Alpha=["Volume 1"], Beta=["Volume 1"])
        return _worker(storage, rows, concurrency=2, script=_script(storage, pages=1))

    def test_a_row_on_a_free_card_is_preferred_over_one_on_a_busy_card(
        self, storage: Path
    ) -> None:
        worker = self._pinned(storage, "gpu:0", "gpu:1")
        rows = {row.name: row for row in worker.generations}
        # A session is open on card 0; the next slot should not pick card 0's
        # row while card 1 has claimable work.
        worker._open_sessions.add(_HeldSession(rows["hayai-nova"]))  # type: ignore[arg-type]
        proposed = [
            (Path("/library/Alpha/Volume 1.cbz"), rows["hayai-nova"].id),
            (Path("/library/Beta/Volume 1.cbz"), rows["paddle-manga"].id),
        ]
        ordered = worker._in_device_order(proposed, worker._devices_in_use(), None)
        assert [job[1] for job in ordered] == [
            rows["paddle-manga"].id,
            rows["hayai-nova"].id,
        ]

    def test_with_everything_on_one_device_the_order_is_untouched(
        self, storage: Path
    ) -> None:
        worker = self._pinned(storage, "gpu:0", "gpu:0")
        rows = {row.name: row for row in worker.generations}
        worker._open_sessions.add(  # type: ignore[arg-type]
            _HeldSession(rows["hayai-nova"])
        )
        proposed = [
            (Path("/library/Alpha/Volume 1.cbz"), rows["hayai-nova"].id),
            (Path("/library/Beta/Volume 1.cbz"), rows["paddle-manga"].id),
        ]
        assert worker._in_device_order(proposed, worker._devices_in_use(), None) == proposed

    def test_a_session_topping_itself_up_is_never_steered(self, storage: Path) -> None:
        """`claim_for_session` asks for ONE row; preference does not apply."""
        worker = self._pinned(storage, "gpu:0", "gpu:1")
        rows = {row.name: row for row in worker.generations}
        worker._open_sessions.add(  # type: ignore[arg-type]
            _HeldSession(rows["hayai-nova"])
        )
        proposed = [
            (Path("/library/Alpha/Volume 1.cbz"), rows["hayai-nova"].id),
            (Path("/library/Beta/Volume 1.cbz"), rows["paddle-manga"].id),
        ]
        same = worker._in_device_order(proposed, worker._devices_in_use(), rows["hayai-nova"].id)
        assert same == proposed

    def test_a_monolithic_rows_device_is_its_own_stage(self, storage: Path) -> None:
        rows = _gens(
            {**PRIMARY, "pools": {"stage_device": {"mokuro": "gpu:1"}}},
            {**HAYAI, "pools": {"stage_device": {"engine": "gpu:0"}}},
        )
        _library(storage, Alpha=["Volume 1"])
        worker = _worker(storage, rows, concurrency=2, script=_script(storage, pages=1))
        by_name = {row.name: row for row in worker.generations}
        assert worker._engine_device(by_name["mokuro"]) == "gpu:1"
        assert worker._engine_device(by_name["hayai-nova"]) == "gpu:0"


class TestConcurrencyAndShutdown:
    def test_two_slots_open_two_sessions_and_never_share_a_volume(
        self, storage: Path
    ) -> None:
        rows = _gens(PRIMARY, HAYAI, PADDLE)
        _library(storage, Alpha=["Volume 1", "Volume 2"], Beta=["Volume 1", "Volume 2"])
        worker = _worker(
            storage, rows, concurrency=2, script=_script(storage, pages=3, page_delay=0.02)
        )
        volumes_at_once: list[int] = []
        real_submit = worker._submit_session_volume

        def spy(*args: Any, **kwargs: Any) -> bool:
            result = real_submit(*args, **kwargs)
            with worker._lock:
                paths = [path for path, _ in worker._inflight_ocr]
                volumes_at_once.append(len(paths) - len(set(paths)))
            return result

        worker._submit_session_volume = spy  # type: ignore[method-assign]
        worker._scan_ocr_once()

        assert max(volumes_at_once, default=0) == 0, "no volume is ever claimed twice"
        assert len(_sidecars(storage, ".hayai-nova.mokuro")) == 4
        assert len(_sidecars(storage, ".paddle-manga.mokuro")) == 4

    def test_stop_leaves_no_child_behind(self, storage: Path) -> None:
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1", "Volume 2"])
        worker = _worker(storage, rows, script=_script(storage, pages=500, page_delay=0.02))
        running = threading.Event()
        real_submit = worker._submit_session_volume

        def spy(*args: Any, **kwargs: Any) -> bool:
            result = real_submit(*args, **kwargs)
            running.set()
            return result

        worker._submit_session_volume = spy  # type: ignore[method-assign]
        worker._running = True
        thread = threading.Thread(target=worker._scan_ocr_once, daemon=True)
        thread.start()
        assert running.wait(timeout=20)
        with worker._lock:
            sessions = list(worker._open_sessions)
        assert sessions, "a session was open"
        pids = [session.pid for session in sessions]

        worker.stop()
        thread.join(timeout=30)
        assert not thread.is_alive()
        for session in sessions:
            assert not session.is_alive()
        for pid in pids:
            assert pid is not None
            with pytest.raises(OSError):
                # Reaped and gone: signalling it must fail.
                os.kill(pid, 0)
        # A shutdown is nobody's failure.
        assert _failures(storage) == {}


class TestTheGatesStillHold:
    def test_a_secondary_row_runs_as_a_session_before_the_primary_sidecar(
        self, storage: Path
    ) -> None:
        """No layer waits for the primary: the session reads it, and the
        sidecar names the volume by the id its primary will carry."""
        from mokuro_bunko.metadata.reader_compat import deterministic_uuid

        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1"], primary_done=False)
        worker = _worker(storage, rows, script=_script(storage, pages=1))
        claimed: list[str] = []
        real_claim = worker.claim_next

        def spy(*args: Any, **kwargs: Any) -> Any:
            job = real_claim(*args, **kwargs)
            if job is not None:
                claimed.append(job[1])
            return job

        worker.claim_next = spy  # type: ignore[method-assign]
        with patch.object(worker.processor, "process_library_ocr", return_value=False):
            worker._scan_ocr_once()
        assert sorted(claimed) == sorted([rows[0].id, rows[1].id]), "both rows were offered"
        layer = storage / "library" / "Alpha" / f"Volume 1.{rows[1].name}.mokuro"
        written = json.loads(layer.read_text(encoding="utf-8"))
        assert written["volume_uuid"] == deterministic_uuid("Alpha/Volume 1")

    def test_a_volume_short_of_pages_gets_no_session_work(self, storage: Path) -> None:
        """ADDENDUM 3's skip still applies when the row runs as a session."""
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1", "Volume 2"])
        worker = _worker(storage, rows, script=_script(storage, pages=1))
        short = storage / "library" / "Alpha" / "Volume 1.cbz"
        worker.missing_pages_lookup = lambda cbz: 3 if cbz == short else 0
        worker.processor.missing_pages_lookup = worker.missing_pages_lookup
        for slot in worker._slots:
            slot.processor.missing_pages_lookup = worker.missing_pages_lookup
        worker._scan_ocr_once()

        assert _sidecars(storage, ".hayai-nova.mokuro") == ["Volume 2.hayai-nova.mokuro"]
        assert _failures(storage) == {}
        assert [entry["volume"] for entry in worker.skipped_missing_pages()] == ["Volume 1"]


class TestHoldingTheQueue:
    def test_a_hold_stops_claiming_and_waits_for_what_is_running(
        self, storage: Path
    ) -> None:
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1", "Volume 2", "Volume 3"])
        worker = _worker(storage, rows, script=_script(storage, pages=20, page_delay=0.02))
        running = threading.Event()
        real_submit = worker._submit_session_volume

        def spy(*args: Any, **kwargs: Any) -> bool:
            result = real_submit(*args, **kwargs)
            running.set()
            return result

        worker._submit_session_volume = spy  # type: ignore[method-assign]
        thread = threading.Thread(target=worker._scan_ocr_once, daemon=True)
        thread.start()
        assert running.wait(timeout=20)

        assert worker.hold_queue(timeout=60.0) is True
        assert worker._inflight_ocr == set(), "nothing runs while the queue is held"
        assert worker.claim_next() is None
        # Nothing was killed: the accepted volumes produced their sidecars.
        assert len(_sidecars(storage, ".hayai-nova.mokuro")) >= 1
        worker.release_queue()
        assert worker.queue_held is False
        thread.join(timeout=30)


class TestPreemptionForBenchmark:
    """ADDENDUM 5: a benchmark ends running OCR at once, unrecorded, not waited out.

    `OCRWorker.preempt_for_bench` reuses the EXACT cancel-without-failure
    mechanism `apply_settings` uses when a row is removed
    (`OCRWorker.apply_settings`, ~line 549-576 of ``watcher.py``): every job
    still in flight is added to `_cancelled_ocr` BEFORE anything is killed,
    so `finish_ocr_job`'s `_cancelled_ocr` branch records no failure and no
    backoff -- except unconditional, ending every open session and every
    per-volume subprocess, whatever row it belongs to, not only the rows
    whose recipe changed.
    """

    def test_kills_a_running_session_and_the_volume_reappears_pending(
        self, storage: Path
    ) -> None:
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1"])
        script = _script(storage, pages=200, page_delay=0.02)
        worker = _worker(storage, rows, script=script)
        running = threading.Event()
        real_submit = worker._submit_session_volume

        def spy(*args: Any, **kwargs: Any) -> bool:
            result = real_submit(*args, **kwargs)
            running.set()
            return result

        worker._submit_session_volume = spy  # type: ignore[method-assign]
        thread = threading.Thread(target=worker._scan_ocr_once, daemon=True)
        thread.start()
        try:
            assert running.wait(timeout=20)
            time.sleep(0.3)
            session = worker._slots[0].session
            assert session is not None and session.is_alive(), "the session is really running"

            quiet, preempted = worker.preempt_for_bench(timeout=30.0)
            assert quiet is True
            assert preempted == [{"generation": "hayai-nova", "volume": "Volume 1"}]
            assert worker._inflight_ocr == set()
            assert _failures(storage) == {}, "a pre-empted volume is not the volume's failure"
            assert _sidecars(storage, ".hayai-nova.mokuro") == [], "nothing was collected"

            # It reappears as pending AT ONCE -- not hidden behind the hold
            # until the next scan, which never runs while the queue is held.
            pending = {
                (j["series"], j["volume"], j["generation"]) for j in worker.pending_jobs()
            }
            assert ("Alpha", "Volume 1", "hayai-nova") in pending
        finally:
            worker.release_queue()
            worker._stop_requested = True
            thread.join(timeout=30)

    def test_kills_a_running_per_volume_job_and_the_volume_reappears_pending(
        self, storage: Path
    ) -> None:
        """The monolithic path: one subprocess, one volume -- killed the same way."""
        rows = _gens(PRIMARY)  # the primary itself runs the per-volume path
        _library(storage, Alpha=["Volume 1"], primary_done=False)
        worker = _worker(storage, rows)

        def fake_process(cbz: Path, generation: GenerationSpec) -> bool:
            # Started the way a real run starts its subprocess
            # (`_start_process`): a pre-empt that lands before it exists is
            # honoured -- it does not start, or is killed on the spot. It
            # used to be assigned to `_active_process` by hand, which is
            # exactly the window a pre-empt fell through (2 in 10 runs).
            proc = worker.processor._start_process(
                [sys.executable, "-c", "import time; time.sleep(30)"]
            )
            if proc is not None:
                proc.wait()
            return False

        thread = threading.Thread(target=worker._scan_ocr_once, daemon=True)
        with patch.object(worker.processor, "process_library_ocr", side_effect=fake_process):
            thread.start()
            try:
                for _ in range(500):
                    if worker._slots[0].job is not None:
                        break
                    time.sleep(0.02)
                assert worker._slots[0].job is not None, "the per-volume job never started"

                quiet, preempted = worker.preempt_for_bench(timeout=30.0)
                assert quiet is True
                assert preempted == [{"generation": "mokuro", "volume": "Volume 1"}]
                assert worker._inflight_ocr == set()
                assert _failures(storage) == {}, (
                    "a pre-empted volume is not the volume's failure"
                )

                pending = {
                    (j["series"], j["volume"], j["generation"]) for j in worker.pending_jobs()
                }
                assert ("Alpha", "Volume 1", "mokuro") in pending
            finally:
                worker.release_queue()
                worker._stop_requested = True
                thread.join(timeout=30)

    def test_holds_and_releases_the_queue_like_a_settings_change_cancel(
        self, storage: Path
    ) -> None:
        """The hold itself: claiming is blocked while held, free again after."""
        rows = _gens(PRIMARY, HAYAI)
        _library(storage, Alpha=["Volume 1"])
        worker = _worker(storage, rows, script=_script(storage, pages=1))
        assert worker.queue_held is False
        quiet, preempted = worker.preempt_for_bench(timeout=10.0)
        assert quiet is True
        assert preempted == [], "nothing was running to interrupt"
        assert worker.queue_held is True
        assert worker.claim_next() is None, "claiming stays blocked while pre-empted"
        worker.release_queue()
        assert worker.queue_held is False
        assert worker.claim_next() is not None, "free to claim again once released"


class TestStagedRunner:
    def test_the_runner_is_staged_once_per_build_and_old_copies_are_pruned(
        self, storage: Path
    ) -> None:
        from mokuro_bunko.ocr.staging import stage_runner

        first = stage_runner(storage)
        assert first.is_file()
        assert first.parent.name.startswith("runner-")
        stale = storage / ".processing" / "runner-deadbeefdeadbeef"
        stale.mkdir(parents=True)
        (stale / "engine_runner.py").write_text("old", encoding="utf-8")

        again = stage_runner(storage)
        assert again == first, "one directory per content hash"
        assert not stale.exists(), "old builds are pruned"

    def test_a_staged_runner_in_use_is_never_pruned(self, storage: Path) -> None:
        from mokuro_bunko.ocr.staging import (
            hold_staged_runner,
            release_staged_runner,
            stage_runner,
        )

        current = stage_runner(storage)
        busy = storage / ".processing" / "runner-0000000000000000"
        busy.mkdir(parents=True)
        (busy / "engine_runner.py").write_text("in use", encoding="utf-8")
        hold_staged_runner(busy / "engine_runner.py")
        try:
            stage_runner(storage)
            assert busy.exists()
        finally:
            release_staged_runner(busy / "engine_runner.py")
        stage_runner(storage)
        assert not busy.exists()
        assert current.is_file()


def test_the_fake_runner_speaks_the_contract(storage: Path, tmp_path: Path) -> None:
    """The fake itself: everything above rests on it being the protocol.

    A test double that drifts from the contract tests nothing, so its output
    is read here the way the contract writes it -- one JSON object a line on
    stdout, prose only in the session log.
    """
    workspace = tmp_path / "ws"
    workspace.mkdir()
    pages_dir = tmp_path / "pages"
    pages_dir.mkdir()
    script = _script(storage, pages=2)
    env = dict(os.environ, FAKE_RUNNER_SCRIPT=str(script), PYTHONUNBUFFERED="1")
    log = tmp_path / "session.log"
    op = {
        "op": "volume",
        "id": "v1",
        "archive": str(tmp_path / "Vol 1.cbz"),
        "workspace": str(workspace),
        "output": str(workspace / "Vol 1.mokuro"),
        "cache_dir": str(workspace / "cache"),
        "detect_dir": str(workspace / "detect"),
        "log": str(tmp_path / "vol.log"),
        "title": "Alpha",
        "volume": "Vol 1",
        "title_uuid": None,
        "volume_uuid": None,
    }
    proc = subprocess.run(
        [sys.executable, str(FAKE_RUNNER), "--serve", "--session-log", str(log)],
        input=json.dumps(op) + "\n" + json.dumps({"op": "close"}) + "\n",
        capture_output=True,
        text=True,
        env=env,
        timeout=60,
    )
    assert proc.returncode == 0
    events = [json.loads(line) for line in proc.stdout.splitlines() if line.strip()]
    assert [event["event"] for event in events] == [
        "ready",
        "volume_started",
        "page",
        "page",
        "stats",
        "volume_done",
    ]
    assert (workspace / "Vol 1.mokuro").is_file()
    assert "volume v1" in log.read_text(encoding="utf-8")
