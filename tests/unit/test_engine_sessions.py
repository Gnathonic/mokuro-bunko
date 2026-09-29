"""One open pipeline, volume after volume: ``--serve``, the archive source,
the logging seam and the ``--bench`` search.

These run in the dev venv without torch, cv2 or numpy. The pipeline is the
REAL one -- a real ``OpenPipeline`` on the ``ppocr-manga`` road with a faked
``ppocr`` module behind it -- so what is tested is the machinery that runs in
production, not a re-implementation of it. Only two things are stubbed: the
models, and the grab of file descriptor 1 (which a test process cannot give
up and get back).
"""

from __future__ import annotations

import json
import queue
import subprocess
import sys
import textwrap
import threading
import time
import types
import zipfile
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr import engine_runner as runner
from tests.unit.test_engine_runner import FakePPOcrModule, Image, _raw_page

# ---------------------------------------------------------------------------
# The fixtures: a real pipeline with fake models behind it.
# ---------------------------------------------------------------------------

PAGE_NAMES = ("001.webp", "002.webp", "003.webp", "004.webp")


@pytest.fixture(autouse=True)
def _restore_log() -> Any:
    """Every test leaves the runner's logging where it found it."""
    yield
    runner.LOG.to_stdout()


@pytest.fixture
def fake_ppocr(monkeypatch: pytest.MonkeyPatch) -> FakePPOcrModule:
    """The ``ppocr-manga`` road with a faked model pair behind it."""
    from mokuro_bunko.ocr import line_layout

    fixtures = ("manga-page-066", "manga-page-069", "novel-text-013")
    by_name = {
        name: _raw_page(fixtures[i % len(fixtures)]) for i, name in enumerate(PAGE_NAMES)
    }
    fake = FakePPOcrModule(by_name[PAGE_NAMES[0]], by_name=by_name)
    monkeypatch.setattr(
        runner, "load_sibling", lambda name: fake if name == "ppocr" else line_layout
    )
    monkeypatch.setattr(runner, "imread_bgr", lambda path: Image(1115, 1600, path.name))
    monkeypatch.setattr(
        runner, "imdecode_bgr", lambda data: Image(1115, 1600, data.decode("utf-8"))
    )
    monkeypatch.setattr(
        runner,
        "blank_page",
        lambda path: {
            "version": runner.MOKURO_FORMAT_VERSION,
            "img_width": 1115,
            "img_height": 1600,
            "blocks": [],
        },
    )
    monkeypatch.setattr(runner, "blank_page_bytes", lambda data: None)
    return fake


def make_volume(root: Path, name: str, pages: Any = PAGE_NAMES) -> Path:
    """A directory of pages whose bytes name the page (the fakes key on that)."""
    folder = root / name
    folder.mkdir(parents=True, exist_ok=True)
    for page in pages:
        target = folder / page
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(page, encoding="utf-8")
    return folder


def make_archive(root: Path, name: str, members: Any) -> Path:
    archive = root / f"{name}.cbz"
    archive.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(archive, "w") as zf:
        for member in members:
            zf.writestr(member, Path(member).name)
    return archive


class Ops:
    """A stdin that can be written to while the session is running.

    A static string would make every op arrive before the first page does,
    which is the one case ``--serve`` is NOT interesting in.
    """

    def __init__(self) -> None:
        self._queue: queue.Queue[str | None] = queue.Queue()

    def send(self, **op: Any) -> None:
        self._queue.put(json.dumps(op) + "\n")

    def eof(self) -> None:
        self._queue.put(None)

    def __iter__(self) -> Any:
        while True:
            line = self._queue.get()
            if line is None:
                return
            yield line


class Stdout:
    """The protocol stream, captured line by line."""

    def __init__(self) -> None:
        self.lines: list[str] = []
        self._lock = threading.Lock()

    def write(self, text: str) -> None:
        with self._lock:
            self.lines.append(text)

    def flush(self) -> None:
        pass

    def events(self) -> list[dict[str, Any]]:
        out = []
        for chunk in "".join(self.lines).splitlines():
            if not chunk.strip():
                continue
            out.append(json.loads(chunk))  # a non-JSON line fails here, loudly
        return out


def serve_args(tmp_path: Path, **extra: Any) -> Any:
    argv = [
        "--serve",
        "--engine",
        "ppocr-manga",
        "--session-log",
        str(tmp_path / "session.log"),
        "--generator",
        "test",
    ]
    for key, value in extra.items():
        argv += [f"--{key.replace('_', '-')}", str(value)]
    return runner.parse_args(argv)


def volume_op(root: Path, job: str, **extra: Any) -> dict[str, Any]:
    out = root / "out" / job
    return {
        "op": "volume",
        "id": job,
        "output": str(out / f"{job}.mokuro"),
        "cache_dir": str(out / "_ocr"),
        "detect_dir": str(out / "_detect"),
        "log": str(out / "run.log"),
        "title": f"T{job}",
        "volume": f"V{job}",
        "title_uuid": f"tu-{job}",
        "volume_uuid": f"vu-{job}",
        **extra,
    }


def run_session(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, ops: Ops, **extra: Any
) -> tuple[int, Stdout]:
    """Run ``--serve`` in this process, with a fake stdin and stdout."""
    stdout = Stdout()
    monkeypatch.setattr(sys, "stdin", ops)
    code = runner.serve(serve_args(tmp_path, **extra), stdout=stdout)
    return code, stdout


def _crop_nothing(img: Any, blk: dict[str, Any], line_idx: int) -> list[Any]:
    return []


def drive(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, ops: Ops) -> list[dict[str, Any]]:
    code, stdout = run_session(tmp_path, monkeypatch, ops)
    assert code == 0
    return stdout.events()


# ---------------------------------------------------------------------------


class TestTheSessionProtocol:
    """The ops the server sends and the events it gets back."""

    def test_two_volumes_through_one_session_write_two_sidecars(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", input=str(make_volume(tmp_path, "A"))))
        ops.send(**volume_op(tmp_path, "two", input=str(make_volume(tmp_path, "B"))))
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)

        kinds = [e["event"] for e in events]
        assert kinds[0] == "ready"
        done = [e for e in events if e["event"] == "volume_done"]
        assert [e["id"] for e in done] == ["one", "two"]
        for job in ("one", "two"):
            sidecar = json.loads(
                (tmp_path / "out" / job / f"{job}.mokuro").read_text(encoding="utf-8")
            )
            assert sidecar["volume_uuid"] == f"vu-{job}"
            assert [p["img_path"] for p in sidecar["pages"]] == list(PAGE_NAMES)
            assert sum(len(p["blocks"]) for p in sidecar["pages"]) > 0

    def test_ready_says_where_each_model_is(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        """The server never has to re-derive a placement it asked for."""
        ops = Ops()
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        ready = events[0]
        assert ready["event"] == "ready"
        # ppocr-manga is one CPU-only model-bearing stage; post/layout hold no
        # model and so are not placements at all.
        assert ready["stage_device"] == {"detect": "cpu"}
        assert "cpu" in ready["pipeline"]

    def test_one_session_loads_the_models_once(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        """The whole point: three volumes, one model load."""
        loads: list[int] = []
        real = runner.PPOcrPageReader.__init__

        def counted(self: Any, *a: Any, **kw: Any) -> None:
            loads.append(1)
            real(self, *a, **kw)

        monkeypatch.setattr(runner.PPOcrPageReader, "__init__", counted)
        ops = Ops()
        for job in ("a", "b", "c"):
            ops.send(**volume_op(tmp_path, job, input=str(make_volume(tmp_path, job.upper()))))
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        assert len(loads) == 1
        assert len([e for e in events if e["event"] == "volume_done"]) == 3

    def test_every_volume_is_started_once_and_ends_once(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        ops = Ops()
        ops.send(**volume_op(tmp_path, "good", input=str(make_volume(tmp_path, "A"))))
        ops.send(**volume_op(tmp_path, "empty", input=str(tmp_path / "nothing")))
        ops.send(**volume_op(tmp_path, "gone", archive=str(tmp_path / "missing.cbz")))
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        for job in ("good", "empty", "gone"):
            started = [e for e in events if e["event"] == "volume_started" and e["id"] == job]
            ended = [
                e
                for e in events
                if e["event"] in ("volume_done", "volume_failed") and e["id"] == job
            ]
            assert len(started) == 1, job
            assert len(ended) == 1, job
        assert {e["id"] for e in events if e["event"] == "volume_failed"} == {"empty", "gone"}

    def test_a_failed_volume_never_ends_the_session(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        ops = Ops()
        ops.send(**volume_op(tmp_path, "bad", archive=str(tmp_path / "not-a-zip.cbz")))
        (tmp_path / "not-a-zip.cbz").write_text("this is not a zip", encoding="utf-8")
        ops.send(**volume_op(tmp_path, "good", input=str(make_volume(tmp_path, "A"))))
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        assert [e["id"] for e in events if e["event"] == "volume_failed"] == ["bad"]
        assert [e["id"] for e in events if e["event"] == "volume_done"] == ["good"]
        assert (tmp_path / "out" / "good" / "good.mokuro").is_file()

    def test_eof_finishes_what_was_accepted(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        """No ``close`` op at all: stdin simply ends."""
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", input=str(make_volume(tmp_path, "A"))))
        ops.send(**volume_op(tmp_path, "two", input=str(make_volume(tmp_path, "B"))))
        ops.eof()
        events = drive(tmp_path, monkeypatch, ops)
        assert [e["id"] for e in events if e["event"] == "volume_done"] == ["one", "two"]

    def test_a_volume_after_close_is_refused_not_half_run(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", input=str(make_volume(tmp_path, "A"))))
        ops.send(op="close")
        ops.send(**volume_op(tmp_path, "late", input=str(make_volume(tmp_path, "B"))))
        ops.eof()
        events = drive(tmp_path, monkeypatch, ops)
        assert [e["id"] for e in events if e["event"] == "volume_done"] == ["one"]
        assert not (tmp_path / "out" / "late").exists()

    def test_the_ready_line_describes_the_pipeline(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        ops = Ops()
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        ready = events[0]
        assert ready["event"] == "ready"
        assert set(ready["stage_workers"]) == {"detect", "layout"}
        assert set(ready["queue_capacity"]) == {"detect", "layout"}
        assert "detect" in ready["pipeline"]
        assert ready["startup_seconds"] >= 0

    def test_page_events_count_that_volume(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        monkeypatch.setattr(runner, "PIPELINE_STATS_INTERVAL", 0.0)
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", input=str(make_volume(tmp_path, "A"))))
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        pages = [e for e in events if e["event"] == "page"]
        assert pages, "a page event a page, once the interval has passed"
        assert {e["id"] for e in pages} == {"one"}
        assert all(e["total"] == len(PAGE_NAMES) for e in pages)
        assert [e["done"] for e in pages] == sorted(e["done"] for e in pages)
        assert any(e["event"] == "stats" for e in events)
        live = next(e for e in events if e["event"] == "stats")["pipeline"]
        assert set(live) >= {"elapsed_seconds", "items", "stages", "queues", "bottleneck"}


class TestVolumesOverlapInsideThePipeline:
    def test_a_volume_arriving_mid_flight_is_accepted_and_overlaps(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        """The second volume's pages are in the pipeline before the first ends.

        Proven by the detect stage: it must see a page of B before the sink
        has finished A, which is only possible if the pipeline did not drain
        at the volume boundary.
        """
        seen: list[str] = []
        real = runner.PPOcrPageReader.detect_page

        def watched(self: Any, img: Any, engine: Any = None) -> Any:
            seen.append(f"detect:{img.name}")
            time.sleep(0.01)
            return real(self, img, engine)

        monkeypatch.setattr(runner.PPOcrPageReader, "detect_page", watched)
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", input=str(make_volume(tmp_path, "A"))))
        ops.send(**volume_op(tmp_path, "two", input=str(make_volume(tmp_path, "B"))))
        ops.send(op="close")

        stdout = Stdout()
        monkeypatch.setattr(sys, "stdin", ops)
        marks: list[str] = []
        real_assemble = runner.Session._assemble

        def marked(self: Any, run: Any, share: Any, *, summary: bool) -> Any:
            marks.append(f"done:{run.request.id}")
            return real_assemble(self, run, share, summary=summary)

        monkeypatch.setattr(runner.Session, "_assemble", marked)
        assert runner.serve(serve_args(tmp_path), stdout=stdout) == 0
        # every page of both volumes was detected, and at least one page was
        # detected after the first volume's sidecar was already being written
        assert len(seen) == 2 * len(PAGE_NAMES)
        assert marks == ["done:one", "done:two"]

    def test_volumes_fed_back_to_back_report_seconds_that_do_not_overlap(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        """Live (server, mokuro served): two volumes sent together read 39.8 s
        for a volume that took 18.9. The feeder reaches the second volume while
        the first is still in the engine, and its clock started there -- so it
        was charged the first one's tail. A volume's seconds now start no
        earlier than the previous volume of the session finished."""
        real = runner.PPOcrPageReader.detect_page

        def slow(self: Any, img: Any, engine: Any = None) -> Any:
            time.sleep(0.05)
            return real(self, img, engine)

        monkeypatch.setattr(runner.PPOcrPageReader, "detect_page", slow)
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", input=str(make_volume(tmp_path, "A"))))
        ops.send(**volume_op(tmp_path, "two", input=str(make_volume(tmp_path, "B"))))
        ops.send(op="close")

        started = time.monotonic()
        events = drive(tmp_path, monkeypatch, ops)
        wall = time.monotonic() - started

        done = [e for e in events if e["event"] == "volume_done"]
        assert [e["id"] for e in done] == ["one", "two"]
        first, second = (e["seconds"] for e in done)
        # Each page costs the same, so the two volumes cost about the same:
        # the second is not charged the first one's pages as well.
        assert second < first * 1.6, (first, second)
        # Together they are the session's pipeline time, never more than the wall.
        assert first + second <= wall + 0.05, (first, second, wall)

    def test_each_volume_gets_its_own_share_of_the_counters(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        ops = Ops()
        for job in ("one", "two", "three"):
            ops.send(**volume_op(tmp_path, job, input=str(make_volume(tmp_path, job))))
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        done = [e for e in events if e["event"] == "volume_done"]
        assert len(done) == 3
        for event in done:
            stats = event["stats"]
            assert event["pages"] == len(PAGE_NAMES), "the volume's own pages"
            assert stats["elapsed_seconds"] > 0
            assert {s["key"] for s in stats["stages"]} == {"detect", "layout"}
            # RAW COUNTERS, not percentages: the server reads them with the
            # same `summarize` it reads a single-volume run's file with.
            for row in stats["stages"]:
                assert set(row) >= {
                    "key",
                    "name",
                    "device",
                    "workers",
                    "items",
                    "busy_seconds",
                    "blocked_seconds",
                    "starved_seconds",
                }
            # A volume's window always contains the whole of its own volume:
            # it opens when its first page ENTERS the pipeline.
            for row in stats["stages"]:
                assert row["items"] >= len(PAGE_NAMES), row["key"]
            assert runner.summarize(stats) is not None, "the server's reading applies"
        # ...and the window is not the whole session's totals in disguise
        session_pages = 3 * len(PAGE_NAMES)
        detect = [next(s for s in e["stats"]["stages"] if s["key"] == "detect") for e in done]
        assert detect[0]["items"] < session_pages

    def test_a_volumes_share_is_a_difference_of_snapshots(self) -> None:
        stage = runner.Stage(
            runner.StageSpec("detect", "d", runner.DEVICE_CPU, runner.POOLED, 1),
            lambda item, payload: time.sleep(0.002) or payload,
            1,
            2,
        )
        pipeline = runner.StagePipeline([stage])
        stream = pipeline.run(range(30))
        for _ in range(10):
            next(stream)
        first = pipeline.report()
        for _ in range(10):
            next(stream)
        share = pipeline.report().since(first)
        assert share.items == 10
        assert share.stages[0].items == 10
        assert 0 < share.stages[0].busy_seconds < pipeline.report().stages[0].busy_seconds
        assert share.elapsed > 0
        # the shape survives, so the server reads it exactly as it reads a run
        assert set(share.as_dict()) == {
            "elapsed_seconds",
            "items",
            "stages",
            "queues",
            "bottleneck",
        }
        stream.close()
        pipeline.close()

    def test_a_snapshot_of_another_pipeline_is_not_an_earlier_one(self) -> None:
        def make(key: str) -> runner.StagePipeline:
            spec = runner.StageSpec(key, key, runner.DEVICE_CPU, runner.POOLED, 1)
            return runner.StagePipeline([runner.Stage(spec, lambda i, p: p, 1, 1)])

        a, b = make("detect"), make("layout")
        list(a.run([1, 2]))
        list(b.run([1, 2]))
        assert b.report().since(a.report()).stages[0].key == "layout"
        assert b.report().since(a.report()).items == b.report().items


class TestTheArchiveIsTheSource:
    def test_the_page_list_is_what_extracting_would_have_given(self, tmp_path: Path) -> None:
        members = [
            "Vol/010.webp",
            "Vol/002.webp",
            "Vol/notes.txt",
            "Vol/sub/001.PNG",
            "Vol.webp",  # the embedded thumbnail some uploaders ship
            "Vol/009.jpeg",
        ]
        archive = make_archive(tmp_path, "Vol", members)
        extracted = tmp_path / "extracted"
        with zipfile.ZipFile(archive) as zf:
            zf.extractall(extracted)
        (extracted / "Vol.webp").unlink()  # what _extract_and_clean does
        assert runner.archive_pages(archive) == runner.list_pages(extracted)

    def test_the_embedded_thumbnail_is_skipped_and_a_nested_one_is_not(
        self, tmp_path: Path
    ) -> None:
        archive = make_archive(tmp_path, "Vol", ["Vol.webp", "Vol/Vol.webp", "001.webp"])
        pages = [p.as_posix() for p in runner.archive_pages(archive)]
        assert "Vol.webp" not in pages
        assert "Vol/Vol.webp" in pages and "001.webp" in pages

    def test_directory_entries_are_not_pages(self, tmp_path: Path) -> None:
        archive = tmp_path / "Vol.cbz"
        with zipfile.ZipFile(archive, "w") as zf:
            zf.writestr("pages/", "")
            zf.writestr("pages/001.webp", "001.webp")
        assert [p.as_posix() for p in runner.archive_pages(archive)] == ["pages/001.webp"]

    def test_a_session_reads_a_volume_straight_out_of_its_archive(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        archive = make_archive(tmp_path, "Vol", list(PAGE_NAMES))
        ops = Ops()
        ops.send(
            **volume_op(
                tmp_path, "one", archive=str(archive), workspace=str(tmp_path / "ws")
            )
        )
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        assert [e["id"] for e in events if e["event"] == "volume_done"] == ["one"]
        sidecar = json.loads(
            (tmp_path / "out" / "one" / "one.mokuro").read_text(encoding="utf-8")
        )
        assert [p["img_path"] for p in sidecar["pages"]] == list(PAGE_NAMES)

    def test_an_in_process_detector_never_writes_the_page_to_disk(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        """The ppocr roads decode the bytes in memory; nothing is spooled."""
        archive = make_archive(tmp_path, "Vol", list(PAGE_NAMES))
        workspace = tmp_path / "ws"
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", archive=str(archive), workspace=str(workspace)))
        ops.send(op="close")
        drive(tmp_path, monkeypatch, ops)
        assert not workspace.exists() or not list(workspace.rglob("*.webp"))

    def test_a_subprocess_detector_gets_a_file_and_it_is_removed_again(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The adapter road must be handed a PATH, and the disk must stay flat."""
        held: list[int] = []
        seen: list[str] = []

        class FakePool:
            weights = {"fake": "sha"}

            def detect(self, image: Path, destination: Path) -> str:
                assert image.is_file(), "the adapter is handed a real file"
                seen.append(image.name)
                held.append(len(list(image.parent.rglob("*.webp"))))
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text(json.dumps({"blocks": []}), encoding="utf-8")
                return "blocks=0"

            def wait(self) -> None:
                pass

            def close(self) -> None:
                pass

        monkeypatch.setattr(
            runner, "open_detectors", lambda detector, *, workers, device="": FakePool()
        )
        monkeypatch.setattr(
            runner,
            "load_recognizer",
            lambda *a, **kw: types.SimpleNamespace(repos={}, __call__=lambda crops: []),
        )
        monkeypatch.setattr(runner, "imread_bgr", lambda path: Image(100, 200, path.name))
        # The real line crop imports cv2, which lives only in the engines venv;
        # these pages have no blocks, so nothing is ever cropped.
        monkeypatch.setattr(runner, "make_line_crop_fn", lambda *a, **k: _crop_nothing)
        archive = make_archive(tmp_path, "Vol", list(PAGE_NAMES))
        workspace = tmp_path / "ws"
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", archive=str(archive), workspace=str(workspace)))
        ops.send(op="close")
        stdout = Stdout()
        monkeypatch.setattr(sys, "stdin", ops)
        argv = [
            "--serve",
            "--engine",
            "hayai-nova",
            "--detector",
            "ctd",
            "--session-log",
            str(tmp_path / "session.log"),
        ]
        assert runner.serve(runner.parse_args(argv), stdout=stdout) == 0
        # The detect stage is a POOL, so the order pages reach the adapter in
        # is the pool's, not the volume's -- what matters is that every page
        # reached it exactly once, as a real file.
        assert sorted(seen) == list(PAGE_NAMES)
        # never more than a bounded handful on disk at once...
        assert max(held) <= len(PAGE_NAMES)
        # ...and nothing left behind when the volume is done
        assert not list(workspace.rglob("*.webp"))

    def test_a_corrupt_member_costs_that_page_and_no_other(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        archive = make_archive(tmp_path, "Vol", list(PAGE_NAMES))
        real_read = runner.ArchiveReader.read

        def broken(self: Any, rel: Path) -> bytes:
            if rel.name == PAGE_NAMES[1]:
                raise zipfile.BadZipFile("bad member")
            return real_read(self, rel)

        monkeypatch.setattr(runner.ArchiveReader, "read", broken)
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", archive=str(archive), workspace=str(tmp_path / "w")))
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        done = next(e for e in events if e["event"] == "volume_done")
        assert done["failed_pages"] == 1
        sidecar = json.loads(
            (tmp_path / "out" / "one" / "one.mokuro").read_text(encoding="utf-8")
        )
        # the same shape a corrupt file on disk has always produced: the page
        # is gone (nothing honest to write) and the rest of the volume is not
        assert [p["img_path"] for p in sidecar["pages"]] == [
            name for name in PAGE_NAMES if name != PAGE_NAMES[1]
        ]

    def test_an_unreadable_archive_fails_that_volume_only(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        bad = tmp_path / "bad.cbz"
        bad.write_bytes(b"PK\x03\x04 and then nonsense")
        ops = Ops()
        ops.send(**volume_op(tmp_path, "bad", archive=str(bad), workspace=str(tmp_path / "w")))
        ops.send(**volume_op(tmp_path, "good", input=str(make_volume(tmp_path, "A"))))
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        failed = next(e for e in events if e["event"] == "volume_failed")
        assert failed["id"] == "bad" and failed["error"]
        assert [e["id"] for e in events if e["event"] == "volume_done"] == ["good"]

    def test_memory_and_disk_stay_flat_over_a_long_volume(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        """A 200-page archive must not be read into memory to be read at all."""
        pages = tuple(f"{i:04d}.webp" for i in range(200))
        by_name = dict.fromkeys(pages, _raw_page("manga-page-066"))
        fake_ppocr.by_name = by_name
        fake_ppocr.engine.by_name = by_name
        archive = make_archive(tmp_path, "Vol", list(pages))
        live: list[int] = []
        real_put = runner.StageQueue.put

        def counted(self: Any, item: Any) -> bool:
            live.append(self.depth)
            return real_put(self, item)

        monkeypatch.setattr(runner.StageQueue, "put", counted)
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", archive=str(archive), workspace=str(tmp_path / "w")))
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        done = next(e for e in events if e["event"] == "volume_done")
        assert done["pages"] == 200
        # every queue stayed within its own capacity -- nothing accumulated
        assert max(live) <= max(
            runner.stage_capacities(runner.ROAD_LINE, [8, 8])
        ), "the pipeline's tickets bound what is in flight, not the volume's length"


class TestTheLoggingSeam:
    def test_the_single_volume_cli_still_writes_its_lines_to_stdout(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        folder = make_volume(tmp_path, "Vol")
        args = runner.parse_args(
            [
                "--engine", "ppocr-manga",
                "--input", str(folder),
                "--output", str(tmp_path / "Vol.mokuro"),
                "--cache-dir", str(tmp_path / "_ocr"),
            ]
        )  # fmt: skip
        assert runner.run(args) == 0
        out = capsys.readouterr().out
        assert out.splitlines()[-1] == "Processed successfully: 1/1"
        assert "[runner] engine=ppocr-manga" in out
        assert f"[runner] page 1/{len(PAGE_NAMES)}" in out
        assert "[runner] pipeline:" in out

    def test_a_session_writes_nothing_to_stdout_and_everything_to_its_logs(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", input=str(make_volume(tmp_path, "A"))))
        ops.send(**volume_op(tmp_path, "two", input=str(make_volume(tmp_path, "B"))))
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        assert capsys.readouterr().out == "", "the protocol is the only thing on stdout"
        assert events

        session = (tmp_path / "session.log").read_text(encoding="utf-8")
        assert "[runner] engine=ppocr-manga" in session
        assert "[runner] pipeline:" in session
        for job in ("one", "two"):
            volume_log = (tmp_path / "out" / job / "run.log").read_text(encoding="utf-8")
            # every line attributable to the volume, and the summary the
            # server's log parser reads, in the order the CLI prints them
            assert f"[runner] page 1/{len(PAGE_NAMES)}" in volume_log
            assert "[runner] pipeline over" in volume_log
            assert "[runner] wrote " in volume_log
            assert volume_log.splitlines()[-1] == "Processed successfully: 1/1"
            # ...and it says nothing about the OTHER volume
            assert f"{job}.mokuro" in volume_log
            other = "two" if job == "one" else "one"
            assert f"{other}.mokuro" not in volume_log
            # the session log has it all too
            assert f"[runner] wrote {tmp_path / 'out' / job / f'{job}.mokuro'}" in session

    def test_a_page_error_and_its_traceback_land_in_that_volumes_log(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        def explode(self: Any, img: Any, engine: Any = None) -> Any:
            raise RuntimeError(f"no good: {img.name}")

        monkeypatch.setattr(runner.PPOcrPageReader, "detect_page", explode)
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", input=str(make_volume(tmp_path, "A"))))
        ops.send(op="close")
        drive(tmp_path, monkeypatch, ops)
        volume_log = (tmp_path / "out" / "one" / "run.log").read_text(encoding="utf-8")
        assert "[runner] ERROR page 001.webp: no good: 001.webp" in volume_log
        assert "Traceback (most recent call last)" in volume_log

    def test_a_warning_raised_on_a_stage_worker_reaches_the_right_volume(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        """Attribution is per THREAD, so a line from deep in a stage lands right."""
        real = runner.PPOcrPageReader.detect_page

        def noisy(self: Any, img: Any, engine: Any = None) -> Any:
            runner.log(f"[runner] WARN something about {img.name}")
            return real(self, img, engine)

        monkeypatch.setattr(runner.PPOcrPageReader, "detect_page", noisy)
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", input=str(make_volume(tmp_path, "A"))))
        ops.send(**volume_op(tmp_path, "two", input=str(make_volume(tmp_path, "B"))))
        ops.send(op="close")
        drive(tmp_path, monkeypatch, ops)
        for job in ("one", "two"):
            text = (tmp_path / "out" / job / "run.log").read_text(encoding="utf-8")
            assert text.count("[runner] WARN something about") == len(PAGE_NAMES)

    SEIZE_SCRIPT = """
        import os, pathlib, subprocess, sys
        sys.path.insert(0, {parent!r})
        import engine_runner as runner
        protocol = runner.Protocol(runner.seize_stdout(pathlib.Path({log!r})))
        print("a stray print")
        os.write(1, b"a library writing to the descriptor\\n")
        os.write(2, b"and something on stderr\\n")
        subprocess.run([sys.executable, "-c", "print('a child of ours')"], check=True)
        runner.log("[runner] a line of ours")
        protocol.emit("ready", weights={{}})
        """

    def _seize(self, tmp_path: Path, **kw: Any) -> Any:
        script = textwrap.dedent(
            self.SEIZE_SCRIPT.format(
                parent=str(Path(runner.__file__).parent), log=str(tmp_path / "session.log")
            )
        )
        return subprocess.run(
            [sys.executable, "-c", script], stdout=subprocess.PIPE, text=True, check=True, **kw
        )

    def test_stdout_is_taken_away_from_everything_but_the_protocol(
        self, tmp_path: Path
    ) -> None:
        """fd 1, not just ``sys.stdout``: a library and a child are on it too.

        In a subprocess, because a test process cannot hand fd 1 over and get
        it back -- and because inheriting the descriptor is the case that
        matters.
        """
        result = self._seize(tmp_path, stderr=subprocess.PIPE)
        assert [json.loads(line) for line in result.stdout.splitlines() if line.strip()] == [
            {"event": "ready", "weights": {}}
        ]
        text = (tmp_path / "session.log").read_text(encoding="utf-8")
        for line in (
            "a stray print",
            "a library writing to the descriptor",
            "a child of ours",
            "[runner] a line of ours",
        ):
            assert line in text, line
        # stderr had somewhere of its own to go, so it was left alone
        assert "and something on stderr" in result.stderr
        assert "and something on stderr" not in text

    def test_a_caller_that_merged_stderr_into_the_pipe_cannot_corrupt_it(
        self, tmp_path: Path
    ) -> None:
        """Then, and only then, fd 2 goes to the log as well."""
        result = self._seize(tmp_path, stderr=subprocess.STDOUT)
        assert [json.loads(line) for line in result.stdout.splitlines() if line.strip()] == [
            {"event": "ready", "weights": {}}
        ]
        assert "and something on stderr" in (tmp_path / "session.log").read_text(
            encoding="utf-8"
        )


class TestTheSessionAndTheCliShareTheirPieces:
    def test_the_cli_is_one_session_with_one_volume(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        """The same sidecar bytes, whichever entry point wrote them."""
        folder = make_volume(tmp_path, "Vol")
        args = runner.parse_args(
            [
                "--engine", "ppocr-manga",
                "--input", str(folder),
                "--output", str(tmp_path / "cli" / "Vol.mokuro"),
                "--cache-dir", str(tmp_path / "cli" / "_ocr"),
                "--detect-dir", str(tmp_path / "cli" / "_detect"),
                "--title", "Tone", "--volume", "Vone",
                "--title-uuid", "tu-one", "--volume-uuid", "vu-one",
                "--generator", "test",
            ]
        )  # fmt: skip
        assert runner.run(args) == 0
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", input=str(folder)))
        ops.send(op="close")
        drive(tmp_path, monkeypatch, ops)
        assert (tmp_path / "cli" / "Vol.mokuro").read_bytes() == (
            tmp_path / "out" / "one" / "one.mokuro"
        ).read_bytes()

    def test_a_session_is_not_sized_on_the_first_volume_that_turns_up(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        """The CLI's "never wider than the pages" guard is the CLI's alone."""
        config = runner.SessionConfig(engine="ppocr-manga", detector="ppocr-manga")
        capped = runner.OpenPipeline(config, page_cap=1)
        try:
            assert capped.host_budget == 1
        finally:
            capped.pipeline.close()
            capped.close()
        session = runner.OpenPipeline(config)
        try:
            assert session.host_budget == runner.host_worker_budget()
        finally:
            session.pipeline.close()
            session.close()


class TestTheModeFlags:
    def test_serve_and_bench_need_a_session_log(self) -> None:
        with pytest.raises(SystemExit):
            runner.parse_args(["--engine", "ppocr-manga", "--serve"])
        with pytest.raises(SystemExit):
            runner.parse_args(["--engine", "ppocr-manga", "--bench", "--input", "/tmp"])

    def test_a_bench_derives_its_own_widths(self) -> None:
        with pytest.raises(SystemExit):
            runner.parse_args(
                [
                    "--engine", "ppocr-manga", "--bench", "--input", "/tmp",
                    "--session-log", "/tmp/x.log", "--stage-workers", "detect=4",
                ]
            )  # fmt: skip

    def test_the_single_volume_cli_still_demands_its_paths(self) -> None:
        with pytest.raises(SystemExit):
            runner.parse_args(["--engine", "ppocr-manga", "--input", "/tmp"])

    def test_serve_does_not_demand_them(self, tmp_path: Path) -> None:
        args = runner.parse_args(
            ["--engine", "ppocr-manga", "--serve", "--session-log", str(tmp_path / "s.log")]
        )
        assert args.serve and not args.input and not args.output

    def test_the_command_line_the_server_builds(self, tmp_path: Path) -> None:
        """Exactly what ``OCRProcessor`` sends, so a drift here is a failed session."""
        session = runner.parse_args(
            [
                "--serve", "--engine", "hayai-nova", "--detector", "ctd",
                "--generator", "mokuro-bunko 0.3.6",
                "--session-log", str(tmp_path / "s.log"), "--patches", "512",
                "--stage-workers", "detect=3,post=2", "--queue-capacity", "detect=4",
            ]
        )  # fmt: skip
        assert session.serve and session.engine == "hayai-nova"
        assert session.generator == "mokuro-bunko 0.3.6"
        assert session.stage_workers == "detect=3,post=2"
        benchmark = runner.parse_args(
            [
                "--bench", "--engine", "hayai-nova", "--detector", "ctd",
                "--input", str(tmp_path),
                "--session-log", str(tmp_path / "s.log"),
                "--bench-max-trials", "8", "--bench-budget-seconds", "900",
                "--patches", "512",
            ]
        )  # fmt: skip
        assert benchmark.bench and benchmark.bench_max_trials == 8
        assert benchmark.bench_budget_seconds == 900.0


class TestTheOpTheServerSends:
    def test_null_uuids_are_minted_and_missing_directories_are_made(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        """Everything under a workspace the server has only just created."""
        workspace = tmp_path / "ws" / "v1"
        op = {
            "op": "volume",
            "id": "v1",
            "workspace": str(workspace),
            "output": str(workspace / "out" / "Vol.mokuro"),
            "cache_dir": str(workspace / "_ocr"),
            "detect_dir": str(workspace / "_detect"),
            "log": str(workspace / "logs" / "run.log"),
            "title": "A Title",
            "volume": "A Volume",
            "title_uuid": None,
            "volume_uuid": None,
            "input": str(make_volume(tmp_path, "A")),
        }
        ops = Ops()
        ops.send(**op)
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        assert [e["id"] for e in events if e["event"] == "volume_done"] == ["v1"]
        sidecar = json.loads(
            (workspace / "out" / "Vol.mokuro").read_text(encoding="utf-8")
        )
        assert sidecar["title"] == "A Title" and sidecar["volume"] == "A Volume"
        assert len(sidecar["title_uuid"]) == 36 and len(sidecar["volume_uuid"]) == 36

    def test_close_and_eof_together_are_one_close(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", input=str(make_volume(tmp_path, "A"))))
        ops.send(op="close")
        ops.eof()
        events = drive(tmp_path, monkeypatch, ops)
        assert [e["id"] for e in events if e["event"] == "volume_done"] == ["one"]

    def test_an_id_that_cannot_be_keyed_by_is_refused_in_the_log_only(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        """Answering would tell the server something about the WRONG job.

        Every other failure gets its pair of events, because the server has a
        job waiting on that id. A MISSING id has no job to answer for, and a
        REPEATED one names a job that is already running -- failing it would
        fail the running one.
        """
        ops = Ops()
        nameless = volume_op(tmp_path, "one", input=str(make_volume(tmp_path, "A")))
        nameless["id"] = ""
        ops.send(**nameless)
        ops.send(**volume_op(tmp_path, "two", input=str(make_volume(tmp_path, "B"))))
        ops.send(**volume_op(tmp_path, "two", input=str(make_volume(tmp_path, "B"))))
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        assert [e["id"] for e in events if e["event"] == "volume_done"] == ["two"]
        assert not [e for e in events if e["event"] == "volume_failed"]
        session = (tmp_path / "session.log").read_text(encoding="utf-8")
        assert "a volume op needs an id" in session
        assert "is already in this session" in session

    def test_any_other_bad_op_still_gets_its_pair_of_events(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        """A job the server is waiting on must always be answered."""
        ops = Ops()
        broken = volume_op(tmp_path, "one", input=str(make_volume(tmp_path, "A")))
        del broken["cache_dir"]  # a field the op is required to carry
        ops.send(**broken)
        ops.send(**volume_op(tmp_path, "two", input=str(make_volume(tmp_path, "B"))))
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        pair = [e for e in events if e.get("id") == "one"]
        assert [e["event"] for e in pair] == ["volume_started", "volume_failed"]
        assert pair[0]["pages"] == 0 and pair[1]["error"]
        assert [e["id"] for e in events if e["event"] == "volume_done"] == ["two"]

    def test_a_volume_op_with_neither_archive_nor_input_fails_that_volume_by_name(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one"))
        ops.send(**volume_op(tmp_path, "two", input=str(make_volume(tmp_path, "B"))))
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        pair = [e for e in events if e.get("id") == "one"]
        assert [e["event"] for e in pair] == ["volume_started", "volume_failed"]
        assert pair[1]["error"] == "volume one names no archive and no input directory"
        assert [e["id"] for e in events if e["event"] == "volume_done"] == ["two"]

    def test_the_stem_on_the_op_decides_the_thumbnail_and_the_fallback_title(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        """A processor's runner reads /proc/<pid>/fd/<n>: the NAME it reads
        must never be the thumbnail's key, the title, or a page."""
        made = make_archive(tmp_path, "Real Stem", [*PAGE_NAMES, "Real Stem.webp"])
        opaque = tmp_path / "fd" / "7"
        opaque.parent.mkdir()
        made.rename(opaque)
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", archive=str(opaque), stem="Real Stem",
                             title=None, volume=None, workspace=str(tmp_path / "ws")))
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        started = next(e for e in events if e["event"] == "volume_started")
        assert started["pages"] == len(PAGE_NAMES), "the thumbnail, keyed by the op's stem"
        sidecar = json.loads((tmp_path / "out" / "one" / "one.mokuro").read_text("utf-8"))
        assert [p["img_path"] for p in sidecar["pages"]] == list(PAGE_NAMES)
        assert sidecar["title"] == "Real Stem" and sidecar["volume"] == "Real Stem"

    def test_an_archive_with_no_pages_is_named_by_its_stem(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule
    ) -> None:
        made = make_archive(tmp_path, "Real Stem", ["notes.txt"])
        opaque = tmp_path / "fd" / "7"
        opaque.parent.mkdir()
        made.rename(opaque)
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", archive=str(opaque), stem="Real Stem"))
        ops.send(op="close")
        events = drive(tmp_path, monkeypatch, ops)
        failed = next(e for e in events if e["event"] == "volume_failed")
        assert failed["error"] == "no page images found in Real Stem.cbz"

    @pytest.mark.parametrize(
        "retired",
        [
            {"op": "page", "id": "one", "index": 0, "path": "/x/000.jpg"},
            {"op": "end", "id": "one"},
            {"pages": "stream", "page_count": 3},
        ],
    )
    def test_a_retired_streamed_op_trips_one_fatal_and_ends_the_op_loop(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_ppocr: FakePPOcrModule,
        retired: dict[str, Any],
    ) -> None:
        """A processor older than this runner -- a deploy done out of order
        -- is told so ONCE, instead of a failure record per volume."""
        ops = Ops()
        if retired.get("pages") == "stream":
            ops.send(**volume_op(tmp_path, "one", **retired))
        else:
            ops.send(**retired)
        # Whatever comes after is never read.
        ops.send(**volume_op(tmp_path, "two", input=str(make_volume(tmp_path, "B"))))
        ops.eof()
        code, stdout = run_session(tmp_path, monkeypatch, ops)
        events = stdout.events()
        fatal = [e for e in events if e["event"] == "fatal"]
        assert len(fatal) == 1
        assert "takes archives only" in fatal[0]["error"]
        assert "stop it, update it, start it again" in fatal[0]["error"]
        assert not [e for e in events if e["event"] in ("volume_failed", "volume_started")]
        assert code == 1
        assert "takes archives only" in (tmp_path / "session.log").read_text("utf-8")

    def test_the_spooled_pages_are_gone_before_volume_done_is_said(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The server deletes the workspace the moment it reads that line."""
        left: list[int] = []

        class FakePool:
            weights: dict[str, str] = {}

            def detect(self, image: Path, destination: Path) -> str:
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text(json.dumps({"blocks": []}), encoding="utf-8")
                return "blocks=0"

            def wait(self) -> None:
                pass

            def close(self) -> None:
                pass

        monkeypatch.setattr(
            runner, "open_detectors", lambda detector, *, workers, device="": FakePool()
        )
        monkeypatch.setattr(
            runner,
            "load_recognizer",
            lambda *a, **kw: types.SimpleNamespace(repos={}, __call__=lambda crops: []),
        )
        monkeypatch.setattr(runner, "imread_bgr", lambda path: Image(100, 200, path.name))
        # The real line crop imports cv2, which lives only in the engines venv;
        # these pages have no blocks, so nothing is ever cropped.
        monkeypatch.setattr(runner, "make_line_crop_fn", lambda *a, **k: _crop_nothing)
        workspace = tmp_path / "ws"
        real_emit = runner.Protocol.emit

        def watched(self: Any, event: str, **fields: Any) -> None:
            if event == "volume_done":
                left.append(len(list(workspace.rglob("*.webp"))))
            real_emit(self, event, **fields)

        monkeypatch.setattr(runner.Protocol, "emit", watched)
        archive = make_archive(tmp_path, "Vol", list(PAGE_NAMES))
        ops = Ops()
        ops.send(**volume_op(tmp_path, "one", archive=str(archive), workspace=str(workspace)))
        ops.send(op="close")
        stdout = Stdout()
        monkeypatch.setattr(sys, "stdin", ops)
        argv = [
            "--serve", "--engine", "hayai-nova", "--detector", "ctd",
            "--session-log", str(tmp_path / "session.log"),
        ]  # fmt: skip
        assert runner.serve(runner.parse_args(argv), stdout=stdout) == 0
        assert left == [0], "nothing of ours is left in the workspace by then"


# ---------------------------------------------------------------------------
# ``--bench``: the search, on scripted throughput curves.
# ---------------------------------------------------------------------------


def stage_rows(widths: dict[str, int], *, starved: str = "", busy: str = "") -> dict[str, Any]:
    """A reading that names ONE stage to widen, or none at all."""
    keys = list(widths)
    rows = []
    for index, key in enumerate(keys):
        rows.append(
            {
                "key": key,
                "workers": widths[key],
                "fused": False,
                "device_bound": False,
                "items": 100,
                "busy_pct": 90.0 if key == busy else 40.0,
                "starved_pct": 50.0 if key == starved and index > 0 else 0.0,
                "blocked_pct": 0.0,
                "queue": {"name": f"{key}->next", "capacity": 1},
            }
        )
    return {"stages": rows}


class ScriptedBench(runner.BenchRun):
    """The real search, over a throughput curve a test writes."""

    def __init__(self, specs: Any, budget: int, curve: Any, *, max_trials: int = 8) -> None:
        args = types.SimpleNamespace(
            bench_max_trials=max_trials, bench_budget_seconds=900.0
        )
        self.recorded: list[dict[str, Any]] = []
        super().__init__(args, _Recorder(self.recorded))
        self.pipe = types.SimpleNamespace(
            specs=specs, host_budget=budget, caps=[1] * len(specs), road=runner.ROAD_RECONCILED
        )
        self.curve = curve

    def _trial(self, widths: Any, *, note: str) -> runner.BenchTrial:
        rate, reading = self.curve(tuple(widths))
        trial = runner.BenchTrial(
            n=len(self.trials) + 1,
            note=note,
            stage_workers=self._map(widths),
            queue_capacity={},
            stage_device={spec.key: spec.device for spec in self.pipe.specs},
            seconds=10.0,
            # A window long enough to decide on (ADDENDUM 9): what these
            # tests are about is the SEARCH, and the search only runs on
            # trials it is allowed to compare.
            window=runner.BenchWindow(
                pages_per_second=rate,
                window_seconds=21.0,
                pages_measured=int(rate * 21) + 1,
                passes=2,
                short_window=False,
                first_emission_at=12.0,
                last_emission_at=33.0,
            ),
            accepted=True,
            verdict=runner.pipeline_verdict(reading),
            bottleneck=None,
            stages=[],
            queues=[],
            reading=reading,
        )
        self.trials.append(trial)
        return trial


class _Recorder:
    def __init__(self, into: list[dict[str, Any]]) -> None:
        self.into = into

    def emit(self, event: str, **fields: Any) -> None:
        self.into.append({"event": event, **fields})


SPECS = (
    runner.StageSpec("detect", "detect", runner.DEVICE_CPU, runner.POOLED, 0.2),
    runner.StageSpec("engine", "engine", runner.DEVICE_GPU, runner.DEVICE_BOUND, 0.3),
    runner.StageSpec("post", "post", runner.DEVICE_CPU, runner.POOLED, 0.01),
)


class TestTheBenchSearch:
    def test_widening_is_kept_while_it_pays_and_reverted_when_it_does_not(self) -> None:
        """The verdict keeps asking; the numbers are what stop the search."""
        rates = {(1, 1, 1): 1.0, (2, 1, 1): 1.5, (3, 1, 1): 2.0, (4, 1, 1): 2.01}

        def curve(widths: tuple[int, ...]) -> Any:
            keys = dict(zip(("detect", "engine", "post"), widths, strict=True))
            return rates[widths], stage_rows(keys, starved="engine")

        bench = ScriptedBench(SPECS, budget=8, curve=curve)
        baseline, best, widths = bench.search([1, 1, 1])
        assert baseline.stage_workers == {"detect": 1, "engine": 1, "post": 1}
        assert best.pages_per_second == 2.0
        assert widths == [3, 1, 1], "the +0.5% step is not kept"
        # the step that did not pay is still REPORTED, and reported as rejected
        assert [(t.n, t.stage_workers["detect"], t.accepted) for t in bench.trials] == [
            (1, 1, True),
            (2, 2, True),
            (3, 3, True),
            (4, 4, False),
        ]
        assert [e["event"] for e in bench.recorded] == ["bench_trial"] * 4
        assert bench.recorded[-1]["accepted"] is False

    def test_the_search_stops_when_the_verdict_stops_asking(self) -> None:
        rates = {(1, 1, 1): 1.0, (2, 1, 1): 1.5, (3, 1, 1): 2.0, (4, 1, 1): 4.0}

        def curve(widths: tuple[int, ...]) -> Any:
            keys = dict(zip(("detect", "engine", "post"), widths, strict=True))
            # the engine stops waiting on detect once detect is wide enough,
            # so the pipeline is balanced and asks for nothing more -- even
            # though a wider detect would (here) have been faster still
            starved = "engine" if widths[0] < 3 else ""
            return rates[widths], stage_rows(keys, starved=starved)

        bench = ScriptedBench(SPECS, budget=8, curve=curve)
        _baseline, _best, widths = bench.search([1, 1, 1])
        assert widths == [3, 1, 1]
        assert len(bench.trials) == 3

    def test_a_stage_at_its_structural_ceiling_is_never_widened(self) -> None:
        def curve(widths: tuple[int, ...]) -> Any:
            keys = dict(zip(("detect", "engine", "post"), widths, strict=True))
            # the reading blames the GPU stage, which is one model on one card
            return 1.0, stage_rows(keys, starved="post")

        bench = ScriptedBench(SPECS, budget=8, curve=curve)
        _baseline, _best, widths = bench.search([1, 1, 1])
        assert widths == [1, 1, 1]
        assert len(bench.trials) == 1, "nothing to try: the verdict names a fixed stage"

    def test_the_host_budget_caps_the_search(self) -> None:
        rates = {(1, 1, 1): 1.0, (2, 1, 1): 2.0, (3, 1, 1): 4.0}

        def curve(widths: tuple[int, ...]) -> Any:
            keys = dict(zip(("detect", "engine", "post"), widths, strict=True))
            return rates[widths], stage_rows(keys, starved="engine")

        bench = ScriptedBench(SPECS, budget=2, curve=curve)
        _baseline, _best, widths = bench.search([1, 1, 1])
        assert widths == [2, 1, 1], "widened to the budget and no further"

    def test_the_trial_cap_stops_the_search(self) -> None:
        def curve(widths: tuple[int, ...]) -> Any:
            keys = dict(zip(("detect", "engine", "post"), widths, strict=True))
            return 1.0 + widths[0], stage_rows(keys, starved="engine")

        bench = ScriptedBench(SPECS, budget=64, curve=curve, max_trials=3)
        _baseline, _best, widths = bench.search([1, 1, 1])
        assert len(bench.trials) <= 3
        assert widths == [3, 1, 1]

    def test_a_width_that_is_not_being_used_is_given_back(self) -> None:
        """Same speed for fewer cores is a win, and is reported as one."""
        rates = {(4, 1, 1): 2.0, (3, 1, 1): 2.0, (2, 1, 1): 1.995, (1, 1, 1): 1.0}

        def curve(widths: tuple[int, ...]) -> Any:
            keys = dict(zip(("detect", "engine", "post"), widths, strict=True))
            return rates[widths], stage_rows(keys, busy="engine")

        bench = ScriptedBench(SPECS, budget=8, curve=curve)
        _baseline, best, widths = bench.search([4, 1, 1])
        assert widths == [2, 1, 1], "narrowed while throughput held within 1%"
        assert best.pages_per_second == pytest.approx(1.995)
        assert [t.stage_workers["detect"] for t in bench.trials] == [4, 3, 2, 1]
        assert [t.accepted for t in bench.trials] == [True, True, True, False]

    def test_narrowing_is_judged_against_the_peak_not_the_step_before(self) -> None:
        """A chain of "within 1%" steps must not walk the pipeline down."""
        rates = {(4, 1, 1): 2.0, (3, 1, 1): 1.99, (2, 1, 1): 1.975, (1, 1, 1): 1.0}

        def curve(widths: tuple[int, ...]) -> Any:
            keys = dict(zip(("detect", "engine", "post"), widths, strict=True))
            return rates[widths], stage_rows(keys, busy="engine")

        bench = ScriptedBench(SPECS, budget=8, curve=curve)
        _baseline, _best, widths = bench.search([4, 1, 1])
        # 3 holds against the peak of 2.0; 2 does not (1.2% down), so it stops
        assert widths == [3, 1, 1]

    def test_a_balanced_pipeline_is_left_alone(self) -> None:
        def curve(widths: tuple[int, ...]) -> Any:
            keys = dict(zip(("detect", "engine", "post"), widths, strict=True))
            return 1.0, stage_rows(keys)

        bench = ScriptedBench(SPECS, budget=8, curve=curve)
        _baseline, _best, widths = bench.search([1, 1, 1])
        assert widths == [1, 1, 1]
        assert len(bench.trials) == 1

    def test_nothing_to_widen_is_a_road_with_no_pooled_stage(self) -> None:
        bound = (
            runner.StageSpec("detect", "d", runner.DEVICE_CPU, runner.DEVICE_BOUND, 0.2),
            runner.StageSpec("engine", "e", runner.DEVICE_GPU, runner.DEVICE_BOUND, 0.3),
        )
        bench = ScriptedBench(bound, budget=8, curve=lambda w: (1.0, {"stages": []}))
        assert not bench._tunable([1, 1])
        wide = ScriptedBench(SPECS, budget=8, curve=lambda w: (1.0, {"stages": []}))
        assert wide._tunable([1, 1, 1])
        assert not wide._tunable([8, 1, 8]), "every stage already at the budget"


class TestTheBenchPlacementTrial:
    """Addendum 7: after the widths, the tuner tries the detector elsewhere."""

    @staticmethod
    def _placing(curve: Any, devices: list[str], **kw: Any) -> ScriptedBench:
        """A scripted bench whose pipeline can move its detect stage."""
        bench = ScriptedBench(SPECS, budget=8, curve=curve, **kw)
        state = {"device": "gpu:0", "widths": {"gpu:0": [1, 1, 1], "cpu": [3, 1, 1]}}
        bench.moves: list[str] = []  # type: ignore[attr-defined]

        def move(device: str) -> None:
            state["device"] = device
            bench.moves.append(device)  # type: ignore[attr-defined]
            bench.pipe.widths = list(state["widths"][device])
            bench.pipe.specs = tuple(
                spec._replace(device=device) if spec.key == "detect" else spec
                for spec in SPECS
            )

        bench.pipe.detect_devices = lambda: [d for d in devices if d != state["device"]]
        bench.pipe.move_detect = move
        bench.pipe._stage_device = lambda key: (
            state["device"] if key == "detect" else "gpu:0"
        )
        bench.pipe.widths = list(state["widths"]["gpu:0"])
        return bench

    def test_a_faster_placement_is_kept_and_its_widths_re_searched(self) -> None:
        """Three CPU detectors beat one on the card, and are searched again."""

        def curve(widths: tuple[int, ...]) -> Any:
            keys = dict(zip(("detect", "engine", "post"), widths, strict=True))
            rate = 1.0 if widths[0] == 1 else 2.0 + widths[0] * 0.1
            return rate, stage_rows(keys, starved="engine")

        bench = self._placing(curve, ["cpu", "gpu:0"])
        best, widths = bench.place(bench._trial([1, 1, 1], note="auto"), [1, 1, 1])
        assert bench.moves[0] == "cpu"
        assert bench.pipe._stage_device("detect") == "cpu"
        assert widths[0] >= 3, "the winner's widths are searched from its own derivation"
        assert best.pages_per_second > 1.0
        assert any("detect on cpu" in trial.note for trial in bench.trials)

    def test_a_slower_placement_is_put_back(self) -> None:
        def curve(widths: tuple[int, ...]) -> Any:
            keys = dict(zip(("detect", "engine", "post"), widths, strict=True))
            # The CPU pool is worse whatever its width (measured: on this host
            # ctd on the CPU really is).
            return (1.0 if widths[0] == 1 else 0.4), stage_rows(keys)

        bench = self._placing(curve, ["cpu", "gpu:0"])
        best, widths = bench.place(bench._trial([1, 1, 1], note="auto"), [1, 1, 1])
        assert bench.moves == ["cpu", "gpu:0"], "it goes back where it was"
        assert widths == [1, 1, 1]
        assert best.pages_per_second == 1.0
        rejected = [t for t in bench.recorded if t.get("event") == "bench_trial"]
        assert any(row["accepted"] is False for row in rejected)

    def test_a_road_with_nowhere_to_move_is_left_alone(self) -> None:
        bench = self._placing(lambda w: (1.0, {"stages": []}), [])
        first = bench._trial([1, 1, 1], note="auto")
        best, widths = bench.place(first, [1, 1, 1])
        assert bench.moves == []
        assert (best, widths) == (first, [1, 1, 1])

    def test_the_time_budget_stops_the_placement_search_too(self) -> None:
        bench = self._placing(lambda w: (1.0, {"stages": []}), ["cpu"], max_trials=1)
        first = bench._trial([1, 1, 1], note="auto")
        assert bench._out_of_time()
        bench.place(first, [1, 1, 1])
        assert bench.moves == []


class TestTheVerdictIsReadTheSameWayEverywhere:
    def test_the_runner_and_the_server_read_the_same_numbers(self) -> None:
        """One implementation: the server re-exports the runner's."""
        from mokuro_bunko.ocr import pipeline_stats

        assert pipeline_stats.pipeline_verdict is runner.pipeline_verdict
        assert pipeline_stats.summarize is runner.summarize
        assert pipeline_stats.MIN_PAGES == runner.MIN_PAGES

    def test_the_stage_to_widen_is_derived_with_the_sentence_not_scraped(self) -> None:
        reading = stage_rows({"detect": 1, "engine": 1, "post": 1}, starved="engine")
        assert runner.widen_target(reading) == "detect"
        assert "widen detect" in (runner.pipeline_verdict(reading) or "")

    def test_a_fact_with_no_knob_names_no_stage(self) -> None:
        reading = stage_rows({"detect": 1, "engine": 1}, busy="engine")
        reading["stages"][1]["device_bound"] = True
        verdict = runner.pipeline_verdict(reading)
        assert verdict and "cannot be widened" in verdict
        assert runner.widen_target(reading) is None
