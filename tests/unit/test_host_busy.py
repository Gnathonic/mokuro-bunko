"""A neighbour's load is not learned as the machine's speed (perf diagnosis F5).

The workstation's profile read 4.04 pages/s against a benchmark of 13.5 because
it pooled three uncontended night volumes (11.6 pages/s) with four read
beside three CPU-mode mokuro runs (2.66 pages/s): the same volume took 18.4 s
at night and 57.3 s in the afternoon. So the runner reports the CPU pressure
over each volume's own window, the library does not learn a contended
volume's speed -- not in the rate model, not in the processor's profile --
and the queue card says "host busy" while it lasts.
"""

from __future__ import annotations

import sys
import time
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest

from mokuro_bunko.ocr import engine_runner as runner
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.remote.profiles import ProcessorProfiles
from mokuro_bunko.ocr.watcher import (
    CONTENDED_CPU_PRESSURE,
    CONTENDED_OTHER_CPU,
    OCRWorker,
    _SessionJob,
)
from mokuro_bunko.queue.shape import shape_status


def _psi(path: Path, *, avg10: float, total: int) -> None:
    path.write_text(
        f"some avg10={avg10:.2f} avg60=0.00 avg300=0.00 total={total}\n"
        "full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        encoding="ascii",
    )


class TestTheRunnerMeasuresIt:
    def test_a_volume_s_pressure_is_its_own_window_s(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        psi = tmp_path / "cpu"
        monkeypatch.setattr(runner, "PSI_CPU", psi)
        _psi(psi, avg10=12.0, total=5_000_000)
        run = SimpleNamespace(pressure_mark=None)
        session = SimpleNamespace(pipe=SimpleNamespace(pipeline=SimpleNamespace(
            report=lambda: None)))
        runner.Session.open_window(session, run)  # type: ignore[arg-type]
        assert run.pressure_mark is not None and run.pressure_mark[0] == 5_000_000
        run.pressure_mark = (5_000_000, time.time() - 2.0)
        _psi(psi, avg10=85.0, total=5_000_000 + 1_700_000)  # 1.7 s stalled in 2 s
        assert runner.Session.volume_pressure(run) == pytest.approx(0.85, abs=0.02)  # type: ignore[arg-type]
        assert runner.cpu_pressure_now() == 0.85

    def test_no_psi_no_number(self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setattr(runner, "PSI_CPU", tmp_path / "absent")
        assert runner.cpu_pressure_total() is None
        assert runner.cpu_pressure_now() is None
        run = SimpleNamespace(pressure_mark=(1.0, time.time() - 1))
        assert runner.Session.volume_pressure(run) is None  # type: ignore[arg-type]


def _worker(storage: Path) -> OCRWorker:
    (storage / "library").mkdir(exist_ok=True)
    return OCRWorker(
        storage_path=storage, poll_interval=30.0,
        generations=parse_generation_list([
            {"name": "mokuro", "engine": "mokuro", "primary": True},
            {"name": "hayai", "engine": "hayai-nova"},
        ]),
        engines_python_path=Path(sys.executable), concurrency=1, sessions=True,
    )


def _done(
    worker: OCRWorker,
    pressure: float | None,
    *,
    hardware: str = "desktop",
    other_cpu: float | None = None,
) -> Any:
    row = worker.generations[1]
    cbz = worker.storage_path / "library" / "Alpha" / "Volume 1.cbz"
    cbz.parent.mkdir(parents=True, exist_ok=True)
    cbz.write_bytes(b"not read here")
    volume = SimpleNamespace(id="v1", workspace=worker.storage_path / "ws",
                             output=worker.storage_path / "ws" / "x.mokuro",
                             log=worker.storage_path / "v.log")
    entry = _SessionJob(job=(cbz, row.id), generation=row, volume=volume,  # type: ignore[arg-type]
                        hardware=hardware)
    event: dict[str, Any] = {"event": "volume_done", "id": "v1", "pages": 200, "seconds": 50.0}
    if pressure is not None:
        event["cpu_pressure"] = pressure
    if other_cpu is not None:
        event["other_cpu"] = other_cpu
    worker._collect_session_volume = lambda *a, **k: True  # type: ignore[method-assign]
    worker._handle_session_event(event, row, {"v1": entry}, ["v1"], hardware=hardware)
    return row


class TestTheLibraryDoesNotLearnIt:
    def test_a_contended_volume_moves_no_rate_and_no_profile_speed(
        self, tmp_path: Path
    ) -> None:
        worker = _worker(tmp_path)
        row = _done(worker, CONTENDED_CPU_PRESSURE + 0.25)
        assert worker.rates.throughput(worker._rate_key(row.id, "desktop")) is None
        runs = ProcessorProfiles(tmp_path).row("desktop", row.id,
                                               recipe=row.output_affecting()).runs
        assert runs.get("contended") == 1
        assert "pages_per_second" not in runs and not runs.get("volumes")

    def test_a_calm_volume_is_learned_as_before(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path)
        row = _done(worker, 0.2)
        assert worker.rates.throughput(worker._rate_key(row.id, "desktop")) is not None
        runs = ProcessorProfiles(tmp_path).row("desktop", row.id,
                                               recipe=row.output_affecting()).runs
        assert runs["volumes"] == 1 and runs["pages"] == 200

    def test_an_older_runner_that_says_nothing_is_learned_as_before(
        self, tmp_path: Path
    ) -> None:
        worker = _worker(tmp_path)
        row = _done(worker, None)
        assert worker.rates.throughput(worker._rate_key(row.id, "desktop")) is not None

    def test_the_card_says_host_busy_while_the_pressure_lasts(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path)
        row = worker.generations[1]
        cbz = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        job = (cbz, row.id)
        worker.begin_ocr_job(job, row, slot=0)
        entry = _SessionJob(job=job, generation=row, volume=SimpleNamespace(id="v1"))  # type: ignore[arg-type]
        worker._handle_session_event({"event": "stats", "cpu_pressure": 0.9}, row,
                                     {"v1": entry}, ["v1"])
        assert worker._active_progress[job]["host_busy"] is True
        worker._handle_session_event({"event": "stats", "cpu_pressure": 0.1}, row,
                                     {"v1": entry}, ["v1"])
        assert worker._active_progress[job]["host_busy"] is False
        raw = {"current_jobs": [dict(worker._active_progress[job], host_busy=True)],
               "pending_ocr": []}
        assert shape_status(raw, "normal", admin=False)["machines"][0]["jobs"][0]["host_busy"]
        assert "host_busy" not in shape_status(raw, "minimal", admin=False)["machines"][0][
            "jobs"][0]


# --- the neighbour the pressure could not see -----------------------------------
#
# Measured after the torch thread cap (judge 09-25): a neighbour on half of
# tower's CPUs read 0.29 of CPU pressure before the cap and ~0.01-0.015 after,
# a GPU neighbour 0.01-0.02, and the pressure line (0.6) was only ever crossed
# near full saturation -- live, the guard never fired, and a CPU neighbour's
# load went into the profiles as the machine's speed. So
# the runner also reports what OTHER processes did with the host's CPU over
# the volume's window: the host's busy time (/proc/stat) minus every OCR
# runner's own process tree (this one, a sibling session's, their detector
# and serve children).


def _stat_line(
    pid: int, ppid: int, jiffies: int, comm: str = "python3", start: int = 100
) -> str:
    # utime stime cutime cstime are fields 14-17; split them 4 ways. The
    # start time (field 22, clock ticks after boot) tells a reused pid apart.
    q, r = divmod(jiffies, 4)
    return (
        f"{pid} ({comm}) S {ppid} {pid} {pid} 0 -1 4194304 0 0 0 0 "
        f"{q + r} {q} {q} {q} 20 0 1 0 {start} 1000 100\n"
    )


def _fake_proc(
    root: Path,
    *,
    cpu: tuple[int, ...],
    procs: dict[int, tuple[int, int, str, str]],
    starts: dict[int, int] | None = None,
) -> Path:
    """A /proc: the aggregate ``cpu`` line, and pid -> (ppid, jiffies, comm, argv)."""
    root.mkdir(parents=True, exist_ok=True)
    (root / "stat").write_text(
        "cpu  " + " ".join(str(v) for v in cpu) + "\ncpu0 1 2 3 4 5 6 7 8 0 0\n",
        encoding="ascii",
    )
    for pid, (ppid, jiffies, comm, argv) in procs.items():
        folder = root / str(pid)
        folder.mkdir(exist_ok=True)
        start = (starts or {}).get(pid, 100)
        (folder / "stat").write_text(_stat_line(pid, ppid, jiffies, comm, start),
                                     encoding="utf-8")
        (folder / "cmdline").write_bytes(argv.replace(" ", "\0").encode() + b"\0")
    (root / "self").mkdir(exist_ok=True)  # not a pid: skipped
    return root


RUNNER_ARGV = "/venv/bin/python /s/.processing/runner-ab12/engine_runner.py --serve"


def _ours(ticks: int) -> dict[tuple[int, int], tuple[int, int]]:
    """A runner tree of one process with this many lifetime ticks."""
    return {(100, 100): (ticks, 1)}


def _procs(scale: int = 1) -> dict[int, tuple[int, int, str, str]]:
    return {
        1: (0, 5 * scale, "systemd", "/sbin/init"),
        100: (1, 300 * scale, "python3", RUNNER_ARGV),             # this session's runner
        101: (100, 40 * scale, "python3", "/venv/bin/python ctd.py --serve"),
        102: (101, 10 * scale, "python3", "/venv/bin/python helper.py"),
        200: (1, 70 * scale, "python3.12", RUNNER_ARGV + " --bench"),  # a sibling session
        201: (200, 30 * scale, "python", "/mokuro-env/bin/python -m mokuro.serve"),
        300: (1, 900 * scale, "blender", "blender -b scene.blend"),   # the neighbour
        301: (300, 100 * scale, "python3", "/usr/bin/python3 addon.py"),
    }


class TestTheRunnerSeesItsNeighbours:
    def test_the_host_is_the_aggregate_cpu_line(self, tmp_path: Path) -> None:
        proc = _fake_proc(tmp_path / "proc", cpu=(100, 5, 50, 1000, 20, 3, 2, 1, 0, 0), procs={})
        # busy = user+nice+system+irq+softirq+steal; idle and iowait are not.
        assert runner.host_cpu_jiffies(proc) == (161, 1181)
        assert runner.host_cpu_jiffies(tmp_path / "absent") is None

    def test_ours_is_every_ocr_runner_and_everything_under_it(self, tmp_path: Path) -> None:
        proc = _fake_proc(tmp_path / "proc", cpu=(0,) * 10, procs=_procs())
        # 100 + 101 + 102 (this runner, its detector, that one's child) and
        # 200 + 201 (another session's runner and its serve child); neither
        # the neighbour (300, 301) nor init.
        assert runner.ocr_tree_jiffies(proc) == 300 + 40 + 10 + 70 + 30
        assert runner.ocr_tree_jiffies(tmp_path / "absent") is None

    def test_a_runner_is_known_by_its_command_line_not_its_name(self, tmp_path: Path) -> None:
        # A process's name is its main thread's, which a library may rename
        # (torch's own worker processes go by "pt_main_thread"): a sibling
        # runner renamed so must still be ours, and a kernel thread's empty
        # command line is never read as one.
        procs = _procs()
        procs[200] = (1, 70, "pt_main_thread", RUNNER_ARGV)
        procs[2] = (0, 0, "kthreadd", "")
        procs[50] = (2, 999, "kworker/0:1", "")
        proc = _fake_proc(tmp_path / "proc", cpu=(0,) * 10, procs=procs)
        assert runner.ocr_tree_jiffies(proc) == 300 + 40 + 10 + 70 + 30

    def test_the_share_is_what_the_others_used_of_the_whole_host(self, tmp_path: Path) -> None:
        before = runner.host_sample(
            _fake_proc(tmp_path / "a", cpu=(1000, 0, 0, 9000, 0, 0, 0, 0, 0, 0), procs=_procs())
        )
        after = runner.host_sample(
            _fake_proc(tmp_path / "b", cpu=(3000, 0, 400, 12600, 0, 0, 0, 0, 0, 0),
                       procs=_procs(scale=3))
        )
        assert before is not None and after is not None
        # 2400 busy jiffies of 6000, of which ours grew 450 -> 1350 (+900):
        # others used 1500 of 6000.
        assert runner.other_cpu_share(before, after) == pytest.approx(0.25)
        assert runner.other_cpu_share(None, after) is None
        assert runner.other_cpu_share(after, after) is None  # no time passed

    def test_never_below_nothing_or_above_everything(self) -> None:
        # Ours read later than the host line can come out ahead of it.
        assert runner.other_cpu_share((0, 0, _ours(0)), (100, 1000, _ours(150))) == 0.0
        assert runner.other_cpu_share((0, 0, _ours(0)), (1000, 1000, _ours(0))) == 1.0

    # -- a runner that comes or goes inside the window ---------------------------
    #
    # A sample holds each runner process's LIFETIME ticks, so a plain sum of
    # them falls by a whole lifetime when a sibling exits (another session, an
    # autobench runner, a local slot's job): ours went negative, the share
    # clamped to 1.0, and a volume read alone on the host was called contended.

    def test_a_sibling_runner_that_exits_is_not_a_neighbour(self, tmp_path: Path) -> None:
        procs = {
            1: (0, 5, "systemd", "/sbin/init"),
            100: (1, 40_000, "python3", RUNNER_ARGV),          # this runner
            200: (1, 360_000, "python3", RUNNER_ARGV + " --bench"),  # a sibling
        }
        before = runner.host_sample(_fake_proc(
            tmp_path / "a", cpu=(1_000_000, 0, 0, 4_000_000, 0, 0, 0, 0, 0, 0), procs=procs))
        del procs[200]  # the sibling exits; its lifetime leaves the sum
        procs[100] = (1, 48_000, "python3", RUNNER_ARGV)
        after = runner.host_sample(_fake_proc(
            tmp_path / "b", cpu=(1_008_000, 0, 0, 4_024_000, 0, 0, 0, 0, 0, 0), procs=procs))
        # 8,000 busy ticks of 32,000, every one of them this runner's.
        assert runner.other_cpu_share(before, after) == 0.0

    def test_a_reaped_detector_is_counted_once(self, tmp_path: Path) -> None:
        # The detector reaps its helper and exits, and the runner reaps it:
        # the pair's whole lifetime (40 + 10 before the window, 20 + 5 inside
        # it) moves into the runner's cutime/cstime, and what was already
        # counted before the window must not be counted a second time.
        procs = _procs()
        before = runner.host_sample(_fake_proc(
            tmp_path / "a", cpu=(1000, 0, 0, 9000, 0, 0, 0, 0, 0, 0), procs=procs))
        del procs[101], procs[102]
        procs[100] = (1, 300 + 100 + (40 + 20) + (10 + 5), "python3", RUNNER_ARGV)
        after = runner.host_sample(_fake_proc(
            tmp_path / "b", cpu=(1200, 0, 0, 9800, 0, 0, 0, 0, 0, 0), procs=procs))
        # Ours grew 100 (the runner) + 20 + 5 (the pair's last ticks): 75 of
        # the 200 busy ticks were others', of 1000.
        assert runner.other_cpu_share(before, after) == pytest.approx(0.075)

    def test_a_runner_that_starts_inside_the_window_is_ours(self, tmp_path: Path) -> None:
        procs = _procs()
        before = runner.host_sample(_fake_proc(
            tmp_path / "a", cpu=(1000, 0, 0, 9000, 0, 0, 0, 0, 0, 0), procs=procs))
        procs[400] = (1, 150, "python3", RUNNER_ARGV)  # a new session
        after = runner.host_sample(_fake_proc(
            tmp_path / "b", cpu=(1200, 0, 0, 9800, 0, 0, 0, 0, 0, 0), procs=procs))
        assert runner.other_cpu_share(before, after) == pytest.approx(0.05)

    def test_a_reused_pid_is_a_new_process(self, tmp_path: Path) -> None:
        # The sibling's pid is handed to a fresh runner: its 30 ticks are all
        # new, not a fall of 40 from the old one's 70.
        procs = _procs()
        before = runner.host_sample(_fake_proc(
            tmp_path / "a", cpu=(1000, 0, 0, 9000, 0, 0, 0, 0, 0, 0), procs=procs))
        procs[200] = (1, 30, "python3", RUNNER_ARGV)
        del procs[201]
        after = runner.host_sample(_fake_proc(
            tmp_path / "b", cpu=(1100, 0, 0, 9900, 0, 0, 0, 0, 0, 0), procs=procs,
            starts={200: 5000}))
        # 100 busy of 1000; 30 of them the new runner's: 70 others'.
        assert runner.other_cpu_share(before, after) == pytest.approx(0.07)

    def test_a_volume_s_share_is_its_own_window_s(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(runner, "PSI_CPU", tmp_path / "no-psi")
        monkeypatch.setattr(
            runner, "PROC",
            _fake_proc(tmp_path / "proc", cpu=(1000, 0, 0, 9000, 0, 0, 0, 0, 0, 0),
                       procs=_procs()),
        )
        run = SimpleNamespace(pressure_mark=None, host_mark=None)
        session = SimpleNamespace(pipe=SimpleNamespace(pipeline=SimpleNamespace(
            report=lambda: None)))
        runner.Session.open_window(session, run)  # type: ignore[arg-type]
        assert run.host_mark is not None and run.host_mark[:2] == (1000, 10000)
        assert sum(ticks for ticks, _ in run.host_mark[2].values()) == 450
        monkeypatch.setattr(
            runner, "PROC",
            _fake_proc(tmp_path / "later", cpu=(3000, 0, 400, 12600, 0, 0, 0, 0, 0, 0),
                       procs=_procs(scale=3)),
        )
        assert runner.Session.volume_other_cpu(run) == pytest.approx(0.25)  # type: ignore[arg-type]


    def test_the_live_share_looks_back_about_ten_seconds_sampling_every_five(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # A sample walks /proc, so it is taken at most every HOST_SHARE_EVERY
        # seconds; between samples the last figure stands.
        samples = {0.0: (0, 0, _ours(0)), 5.0: (500, 1000, _ours(0)),
                   10.0: (700, 2000, _ours(100)), 16.0: (1500, 3200, _ours(100))}
        now = {"t": 0.0}
        taken: list[float] = []

        def sample() -> Any:
            taken.append(now["t"])
            return samples[now["t"]]

        monkeypatch.setattr(runner, "host_sample", sample)
        monkeypatch.setattr(runner.time, "time", lambda: now["t"])
        session = runner.Session.__new__(runner.Session)
        session._host_ticks = runner.deque()
        session._host_share = None
        seen = {}
        for t in (0.0, 2.0, 5.0, 7.0, 10.0, 16.0):
            now["t"] = t
            seen[t] = session._recent_other_cpu()
        assert taken == [0.0, 5.0, 10.0, 16.0]
        assert seen[0.0] is None and seen[2.0] is None
        assert seen[5.0] == pytest.approx(0.5) and seen[7.0] == seen[5.0]
        assert seen[10.0] == pytest.approx(0.3)   # against t=0: 600 of 2000
        # Against t=5: 1000 busy of 2200, 100 of it a runner's: 900 of 2200.
        assert seen[16.0] == pytest.approx(900 / 2200, abs=1e-3)


class TestTheLibraryHearsIt:
    def test_a_cpu_neighbour_is_contended_whatever_the_pressure(
        self, tmp_path: Path, caplog: pytest.LogCaptureFixture
    ) -> None:
        worker = _worker(tmp_path)
        logged: list[str] = []
        worker._log = logged.append  # type: ignore[method-assign]
        # A CPU neighbour on 20 of 32 threads, pressure low.
        row = _done(worker, 0.015, other_cpu=0.62)
        assert worker.rates.throughput(worker._rate_key(row.id, "desktop")) is None
        runs = ProcessorProfiles(tmp_path).row("desktop", row.id,
                                               recipe=row.output_affecting()).runs
        assert runs.get("contended") == 1 and not runs.get("volumes")
        assert any("busy host" in line and "other processes used 62% of the CPU" in line
                   for line in logged), logged

    def test_below_the_line_is_learned(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path)
        row = _done(worker, 0.015, other_cpu=CONTENDED_OTHER_CPU - 0.2)
        assert worker.rates.throughput(worker._rate_key(row.id, "desktop")) is not None

    def test_the_line_is_half_the_host(self) -> None:
        # Half of tower's CPUs (24 of 48) took -73% before the thread cap and
        # still -14% after it.
        assert CONTENDED_OTHER_CPU == 0.5

    def test_the_card_says_host_busy_for_a_cpu_neighbour(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path)
        row = worker.generations[1]
        cbz = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        job = (cbz, row.id)
        worker.begin_ocr_job(job, row, slot=0)
        entry = _SessionJob(job=job, generation=row, volume=SimpleNamespace(id="v1"))  # type: ignore[arg-type]
        stats = {"event": "stats", "cpu_pressure": 0.01, "other_cpu": 0.7}
        worker._handle_session_event(stats, row, {"v1": entry}, ["v1"])
        assert worker._active_progress[job]["host_busy"] is True
        stats = {"event": "stats", "cpu_pressure": 0.01, "other_cpu": 0.05}
        worker._handle_session_event(stats, row, {"v1": entry}, ["v1"])
        assert worker._active_progress[job]["host_busy"] is False


from tests.unit.test_engine_sessions import (  # noqa: E402
    Ops,
    _restore_log,  # noqa: F401 - the autouse fixture those helpers rely on
    fake_ppocr,  # noqa: F401 - a fixture
    make_volume,
    run_session,
    volume_op,
)


@pytest.mark.skipif(not runner.PSI_CPU.exists(), reason="this kernel has no PSI")
def test_a_served_volume_reports_its_pressure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: Any  # noqa: F811
) -> None:
    ops = Ops()
    ops.send(**volume_op(tmp_path, "one", input=str(make_volume(tmp_path, "A"))))
    ops.send(op="close")
    code, stdout = run_session(tmp_path, monkeypatch, ops)
    assert code == 0
    (done,) = [e for e in stdout.events() if e["event"] == "volume_done"]
    assert 0.0 <= done["cpu_pressure"] <= 1.0


@pytest.mark.skipif(not (runner.PROC / "stat").exists(), reason="no /proc/stat here")
def test_a_served_volume_reports_what_its_neighbours_used(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: Any  # noqa: F811
) -> None:
    ops = Ops()
    ops.send(**volume_op(tmp_path, "one", input=str(make_volume(tmp_path, "A"))))
    ops.send(op="close")
    code, stdout = run_session(tmp_path, monkeypatch, ops)
    assert code == 0
    (done,) = [e for e in stdout.events() if e["event"] == "volume_done"]
    assert 0.0 <= done["other_cpu"] <= 1.0
