"""The served road: an engine that is a process, and the client that drives it.

Every test here runs the REAL :class:`ServedEngine` against a REAL subprocess
speaking the fork's protocol (``tests/fixtures/serve_stub.py``), so what is
under test is the protocol -- the window, the ordering, the volume framing,
what a failed page costs and what a dead process costs -- and not a mock of
our own assumptions about it. The stub refuses a violation with a ``fatal``,
which is what makes those assertions worth anything.

No models and no torch: the stub answers a page by reading the file it was
handed, so a page's ``result`` names the file the engine really opened.
"""

from __future__ import annotations

import json
import os
import sys
import zipfile
from collections.abc import Sequence
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr import engine_runner as runner

STUB_DIR = Path(__file__).resolve().parent.parent / "fixtures"
STUB_MODULE = "serve_stub"


@pytest.fixture(autouse=True)
def _restore_log() -> Any:
    yield
    runner.LOG.to_stdout()


def stub_env(**extra: str) -> dict[str, str]:
    """The child's environment: the stub on the path, plus its knobs."""
    env = dict(os.environ)
    env["PYTHONPATH"] = os.pathsep.join(
        p for p in (str(STUB_DIR), env.get("PYTHONPATH", "")) if p
    )
    env.update(extra)
    return env


def open_engine(**knobs: str) -> runner.ServedEngine:
    engine = runner.ServedEngine(sys.executable, STUB_MODULE, env=stub_env(**knobs))
    engine.start()
    return engine


def page(volume: runner.VolumePaths, name: str, seq: int, *, last: bool = False) -> runner.PageJob:
    return runner.PageJob(volume, Path(name), seq=seq, last=last)


def spool(root: Path, name: str) -> Path:
    """A page file whose CONTENTS are its name: the stub echoes them back."""
    root.mkdir(parents=True, exist_ok=True)
    target = root / name
    target.write_text(name, encoding="utf-8")
    return target


def volume_paths(tmp_path: Path, volume_id: str = "v1") -> runner.VolumePaths:
    return runner.VolumePaths(
        input_dir=tmp_path / volume_id,
        detect_dir=tmp_path / "detect",
        cache_dir=tmp_path / "cache",
        id=volume_id,
    )


def text_of(result: dict[str, Any]) -> str:
    return str(result["blocks"][0]["lines"][0])


# ---------------------------------------------------------------------------


class TestTheProtocolClient:
    """:class:`ServedEngine` against a process that enforces the contract."""

    def test_it_comes_up_and_reports_what_the_engine_said(self) -> None:
        engine = open_engine(SERVE_STUB_WINDOW="7", SERVE_STUB_DEVICE="cuda")
        try:
            assert engine.ready["event"] == "ready"
            assert engine.version == "0.0.stub"
            assert engine.device == "cuda"
            assert engine.window == 7
        finally:
            engine.close()

    def test_a_page_comes_back_with_what_the_engine_read(self, tmp_path: Path) -> None:
        engine = open_engine()
        paths = volume_paths(tmp_path)
        try:
            tickets = [
                engine.submit(
                    page(paths, name, seq=i, last=i == 2),
                    spool(tmp_path / "v1", name),
                    owned=True,
                )
                for i, name in enumerate(("a.webp", "b.webp", "c.webp"))
            ]
            assert [text_of(t.wait()) for t in tickets] == ["a.webp", "b.webp", "c.webp"]
        finally:
            engine.close()

    def test_the_pages_reach_the_engine_in_the_source_order(self, tmp_path: Path) -> None:
        """The spooling stage is a pool, so they arrive here shuffled.

        The stub refuses an index that is not the next one, so getting this
        wrong is a ``fatal`` and every page fails -- which is the assertion.
        """
        record = tmp_path / "ops.json"
        engine = open_engine(SERVE_STUB_RECORD=str(record), SERVE_STUB_WINDOW="8")
        paths = volume_paths(tmp_path)
        names = [f"{i:03}.webp" for i in range(8)]
        files = {name: spool(tmp_path / "v1", name) for name in names}
        try:
            # Offered in a deliberately awful order; only the last page is
            # offered last, because that is the one carrying ``last``.
            order = [3, 1, 6, 0, 5, 2, 4, 7]
            tickets = {}
            for seq in order:
                name = names[seq]
                tickets[seq] = engine.submit(
                    page(paths, name, seq=seq, last=seq == 7), files[name], owned=True
                )
            assert [text_of(tickets[i].wait()) for i in range(8)] == names
        finally:
            engine.close()
        ops = json.loads(record.read_text(encoding="utf-8"))["ops"]
        sent = [Path(op["path"]).name for op in ops if op["op"] == "page"]
        assert sent == names, "the engine was handed the volume out of order"

    def test_it_never_has_more_pages_outstanding_than_the_window(self, tmp_path: Path) -> None:
        """The window IS the queue on the engine's input side."""
        record = tmp_path / "ops.json"
        # Two at a time, each answered only once its pair arrives: the
        # engine holds on to pages, so an unbounded caller would run past it.
        engine = open_engine(
            SERVE_STUB_WINDOW="2",
            SERVE_STUB_HOLD="2",
            SERVE_STUB_DELAY="0.01",
            SERVE_STUB_RECORD=str(record),
        )
        paths = volume_paths(tmp_path)
        names = [f"{i:03}.webp" for i in range(12)]
        try:
            tickets = [
                engine.submit(
                    page(paths, name, seq=i, last=i == len(names) - 1),
                    spool(tmp_path / "v1", name),
                    owned=True,
                )
                for i, name in enumerate(names)
            ]
            assert [text_of(t.wait()) for t in tickets] == names
        finally:
            engine.close()
        seen = json.loads(record.read_text(encoding="utf-8"))
        assert seen["max_outstanding"] <= 2, seen["max_outstanding"]

    def test_a_page_the_engine_refuses_costs_that_page_and_no_other(
        self, tmp_path: Path
    ) -> None:
        engine = open_engine(SERVE_STUB_FAIL="b.webp")
        paths = volume_paths(tmp_path)
        try:
            names = ("a.webp", "b.webp", "c.webp")
            tickets = [
                engine.submit(
                    page(paths, name, seq=i, last=i == 2),
                    spool(tmp_path / "v1", name),
                    owned=True,
                )
                for i, name in enumerate(names)
            ]
            assert text_of(tickets[0].wait()) == "a.webp"
            with pytest.raises(runner.ServedPageError, match="refused b.webp"):
                tickets[1].wait()
            assert text_of(tickets[2].wait()) == "c.webp"
            # ... and the engine is still there for the next volume
            assert engine.error is None
        finally:
            engine.close()

    def test_a_page_that_never_reached_the_spool_lets_the_order_past_it(
        self, tmp_path: Path
    ) -> None:
        """A skip is not a page: nothing is sent, and page 2 still follows 0."""
        record = tmp_path / "ops.json"
        engine = open_engine(SERVE_STUB_RECORD=str(record))
        paths = volume_paths(tmp_path)
        try:
            first = engine.submit(
                page(paths, "a.webp", seq=0), spool(tmp_path / "v1", "a.webp"), owned=True
            )
            skipped = engine.skip(page(paths, "b.webp", seq=1), OSError("no room on device"))
            third = engine.submit(
                page(paths, "c.webp", seq=2, last=True),
                spool(tmp_path / "v1", "c.webp"),
                owned=True,
            )
            assert text_of(first.wait()) == "a.webp"
            with pytest.raises(OSError, match="no room"):
                skipped.wait()
            assert text_of(third.wait()) == "c.webp"
        finally:
            engine.close()
        ops = json.loads(record.read_text(encoding="utf-8"))["ops"]
        assert [Path(op["path"]).name for op in ops if op["op"] == "page"] == [
            "a.webp",
            "c.webp",
        ]
        assert [op["index"] for op in ops if op["op"] == "page"] == [0, 1]

    def test_the_spool_copy_is_gone_when_its_answer_arrives(self, tmp_path: Path) -> None:
        engine = open_engine()
        paths = volume_paths(tmp_path)
        try:
            ours = spool(tmp_path / "v1", "a.webp")
            theirs = spool(tmp_path / "their-pages", "b.webp")
            mine = engine.submit(page(paths, "a.webp", seq=0), ours, owned=True)
            yours = engine.submit(page(paths, "b.webp", seq=1, last=True), theirs, owned=False)
            mine.wait()
            yours.wait()
            assert not ours.exists(), "a spooled page must not outlive its answer"
            assert theirs.exists(), "a page we did not spool is not ours to delete"
        finally:
            engine.close()

    def test_two_volumes_go_through_one_process_with_one_start(self, tmp_path: Path) -> None:
        record = tmp_path / "ops.json"
        engine = open_engine(SERVE_STUB_RECORD=str(record))
        try:
            for volume_id in ("one", "two"):
                paths = volume_paths(tmp_path, volume_id)
                tickets = [
                    engine.submit(
                        runner.PageJob(paths, Path(name), seq=seq, last=name == "b.webp"),
                        spool(tmp_path / volume_id, name),
                        owned=True,
                    )
                    for seq, name in enumerate(("a.webp", "b.webp"), start=0 if volume_id == "one" else 2)
                ]
                assert [text_of(t.wait()) for t in tickets] == ["a.webp", "b.webp"]
        finally:
            engine.close()
        ops = json.loads(record.read_text(encoding="utf-8"))["ops"]
        kinds = [(op["op"], op.get("volume") or op.get("index")) for op in ops]
        assert kinds == [
            ("begin", "one"),
            ("page", 0),
            ("page", 1),
            ("end", None),
            ("begin", "two"),
            ("page", 0),
            ("page", 1),
            ("end", None),
            ("quit", None),
        ]

    def test_a_process_that_dies_fails_every_page_in_flight_and_is_recorded(
        self, tmp_path: Path
    ) -> None:
        engine = open_engine(SERVE_STUB_FATAL_AFTER="2", SERVE_STUB_HOLD="4", SERVE_STUB_WINDOW="8")
        paths = volume_paths(tmp_path)
        try:
            names = ("a.webp", "b.webp", "c.webp")
            tickets = [
                engine.submit(
                    page(paths, name, seq=i, last=i == 2),
                    spool(tmp_path / "v1", name),
                    owned=True,
                )
                for i, name in enumerate(names)
            ]
            for ticket in tickets:
                with pytest.raises(runner.ServedEngineError):
                    ticket.wait()
            # The session is over, not the page: the driver reads this and
            # ends the run instead of writing a volume of blanks.
            assert engine.error is not None
            # And a page offered afterwards is refused at once rather than
            # waiting for an answer that cannot come.
            late = engine.submit(
                page(paths, "d.webp", seq=3), spool(tmp_path / "v1", "d.webp"), owned=True
            )
            with pytest.raises(runner.ServedEngineError):
                late.wait()
        finally:
            engine.close()

    def test_a_process_that_exits_without_a_word_is_a_dead_engine(self, tmp_path: Path) -> None:
        engine = open_engine(SERVE_STUB_EXIT_AFTER="1", SERVE_STUB_HOLD="4")
        paths = volume_paths(tmp_path)
        try:
            ticket = engine.submit(
                page(paths, "a.webp", seq=0), spool(tmp_path / "v1", "a.webp"), owned=True
            )
            with pytest.raises(runner.ServedEngineError):
                ticket.wait()
            assert engine.error is not None
        finally:
            engine.close()

    def test_no_ready_line_is_a_failure_to_start(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setattr(runner, "SERVE_READY_TIMEOUT", 3.0)
        engine = runner.ServedEngine(
            sys.executable, STUB_MODULE, env=stub_env(SERVE_STUB_NO_READY="1")
        )
        try:
            with pytest.raises(runner.ServedEngineError, match="ready"):
                engine.start()
        finally:
            engine.close()

    def test_a_module_that_is_not_there_is_a_failure_to_start(self) -> None:
        engine = runner.ServedEngine(sys.executable, "no_such_serve_module", env=stub_env())
        try:
            with pytest.raises(runner.ServedEngineError):
                engine.start()
        finally:
            engine.close()

    def test_a_page_with_no_source_numbering_is_refused(self, tmp_path: Path) -> None:
        """A source that does not stamp its pages cannot be ordered; say so."""
        engine = open_engine()
        paths = volume_paths(tmp_path)
        try:
            with pytest.raises(RuntimeError, match="numbering"):
                engine.submit(
                    runner.PageJob(paths, Path("a.webp")),
                    spool(tmp_path / "v1", "a.webp"),
                    owned=True,
                )
        finally:
            engine.close()

    def test_closing_leaves_no_working_directory_behind(self) -> None:
        engine = open_engine()
        scratch = engine.cwd
        assert scratch is not None and scratch.is_dir()
        engine.close()
        assert not scratch.exists()

    def test_the_engine_never_sees_our_own_import_path(self) -> None:
        """``python -m`` puts the working directory first on sys.path."""
        engine = open_engine()
        try:
            assert engine.cwd is not None
            assert not list(engine.cwd.glob("*.py"))
        finally:
            engine.close()


class TestTheRoad:
    """What the graph says the served road is, before anything runs."""

    def test_the_served_engines_are_the_registry_s(self) -> None:
        from mokuro_bunko.ocr import engines

        for engine_id, (module, args) in runner.SERVED_ENGINES.items():
            spec = engines.get_engine(engine_id)
            assert spec.serve_module == module
            assert args == ()
            assert spec.road == runner.ROAD_SERVED == engines.SERVED_ROAD
        served = {e for e in engines.ENGINE_IDS if engines.get_engine(e).serve_module}
        assert served == set(runner.SERVED_ENGINES)

    def test_a_served_engine_takes_the_served_road_whatever_detector_is_named(self) -> None:
        for detector in runner.DETECTOR_SCRIPTS:
            assert runner.page_road("mokuro", detector) == runner.ROAD_SERVED

    def test_the_road_is_feed_then_the_engine_then_post(self) -> None:
        specs = runner.road_specs(runner.ROAD_SERVED, gpu=True)
        assert [s.key for s in specs] == ["feed", "mokuro", "post"]
        feed, mokuro, post = specs
        assert feed.device == runner.DEVICE_CPU and feed.max_workers is runner.POOLED
        # Card 0 in bunko's own spelling: ``auto`` on a host with a card
        # resolves to an INDEXED id, because with two cards which one it is
        # is the point of the choice (ADDENDUM 7).
        assert mokuro.device == "gpu:0"
        # One process, one model: the graph's width, not the engine's.
        assert mokuro.max_workers == runner.DEVICE_BOUND
        assert post.device == runner.DEVICE_CPU and post.max_workers is runner.POOLED

    def test_the_engine_stage_follows_the_host_when_nothing_has_run(self) -> None:
        assert runner.road_specs(runner.ROAD_SERVED, gpu=False)[1].device == runner.DEVICE_CPU

    def test_the_engine_stage_takes_the_device_the_row_asked_for(self) -> None:
        """ADDENDUM 7 on this road: ``mokuro`` is the stage that holds a model.

        Asked for explicitly it is not a guess, so it survives the ``gpu=False``
        this road is planned with (nothing of the engine is in this process).
        """
        assert runner.model_stages(runner.ROAD_SERVED) == (runner.STAGE_MOKURO,)
        specs = runner.road_specs(runner.ROAD_SERVED, gpu=False, devices={"mokuro": "gpu:1"})
        assert specs[1].device == "gpu:1"
        specs = runner.road_specs(runner.ROAD_SERVED, gpu=True, devices={"mokuro": "cpu"})
        assert specs[1].device == runner.DEVICE_CPU

    def test_the_derived_widths_put_one_page_at_a_time_through_the_engine(self) -> None:
        widths = runner.stage_widths("mokuro", runner.ROAD_SERVED, budget=8)
        assert widths[1] == 1, "the engine stage is one process"
        assert widths[0] >= 1 and widths[2] >= 1

    def test_an_explicit_engine_width_is_still_one_worker(self) -> None:
        """The Workers cell is the ENGINE's pipeline, never a second process."""
        widths = runner.stage_widths(
            "mokuro", runner.ROAD_SERVED, budget=8, workers={"mokuro": 6}
        )
        assert widths[1] == 1

    def test_a_served_engine_needs_the_interpreter_of_its_own_environment(self) -> None:
        with pytest.raises(SystemExit):
            runner.parse_args(
                ["--engine", "mokuro", "--input", "i", "--output", "o", "--cache-dir", "c"]
            )
        args = runner.parse_args(
            [
                "--engine",
                "mokuro",
                "--input",
                "i",
                "--output",
                "o",
                "--cache-dir",
                "c",
                "--mokuro-python",
                "/opt/mokuro/bin/python",
            ]
        )
        assert args.mokuro_python == "/opt/mokuro/bin/python"


class TestTheServedSession:
    """A whole ``--serve`` session on the served road, through the stub."""

    @staticmethod
    def _archive(root: Path, name: str, pages: tuple[str, ...]) -> Path:
        archive = root / f"{name}.cbz"
        with zipfile.ZipFile(archive, "w") as zf:
            for member in pages:
                zf.writestr(member, member)
        return archive

    @staticmethod
    def _serve(
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
        ops: Any,
        **knobs: str,
    ) -> list[dict[str, Any]]:
        from tests.unit.test_engine_sessions import Stdout

        for key, value in stub_env(**knobs).items():
            monkeypatch.setenv(key, value)
        monkeypatch.setitem(runner.SERVED_ENGINES, "mokuro", (STUB_MODULE, ()))
        monkeypatch.setattr(sys, "stdin", ops)
        stdout = Stdout()
        args = runner.parse_args(
            [
                "--serve",
                "--engine",
                "mokuro",
                "--session-log",
                str(tmp_path / "session.log"),
                "--mokuro-python",
                sys.executable,
            ]
        )
        assert runner.serve(args, stdout=stdout) == 0
        return stdout.events()

    def test_two_volumes_through_one_process_write_two_upstream_sidecars(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from tests.unit.test_engine_sessions import Ops, volume_op

        pages = ("001.webp", "002.webp", "003.webp")
        ops = Ops()
        for job in ("one", "two"):
            ops.send(
                **volume_op(
                    tmp_path,
                    job,
                    archive=str(self._archive(tmp_path, job, pages)),
                    workspace=str(tmp_path / "ws" / job),
                )
            )
        ops.send(op="close")
        events = self._serve(tmp_path, monkeypatch, ops, SERVE_STUB_WINDOW="4")

        done = [e for e in events if e["event"] == "volume_done"]
        assert [e["id"] for e in done] == ["one", "two"]
        for job in ("one", "two"):
            sidecar = json.loads(
                (tmp_path / "out" / job / f"{job}.mokuro").read_text(encoding="utf-8")
            )
            # UPSTREAM's keys, in its order, stamped with the version the
            # engine reported -- plus the one thing mokuro's CLI never says,
            # the precision it read at (the row's mode, resolved here).
            assert list(sidecar) == [
                "version",
                "title",
                "title_uuid",
                "volume",
                "volume_uuid",
                "ocr_engine",
                "pages",
            ]
            assert sidecar["ocr_engine"] == {"id": "mokuro", "precision": "fp32"}
            assert sidecar["version"] == "0.0.stub"
            assert [p["img_path"] for p in sidecar["pages"]] == list(pages)
            assert [list(p) for p in sidecar["pages"]] == [
                ["version", "img_width", "img_height", "blocks", "img_path"]
            ] * len(pages)
            assert [p["blocks"][0]["lines"][0] for p in sidecar["pages"]] == list(pages)

    def test_the_session_graph_sizes_the_queue_to_the_engine_s_window(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from tests.unit.test_engine_sessions import Ops

        ops = Ops()
        ops.send(op="close")
        events = self._serve(tmp_path, monkeypatch, ops, SERVE_STUB_WINDOW="9")
        ready = events[0]
        assert ready["event"] == "ready"
        assert ready["stage_workers"] == {"feed": 1, "mokuro": 1, "post": 1}
        assert ready["queue_capacity"]["feed"] == 9
        assert "feed (cpu x1, queue 9)" in ready["pipeline"]

    def test_a_row_may_set_the_engine_s_own_pipeline_width(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """``--stage-workers mokuro=3`` is the engine's ``--num_workers``."""
        seen: list[list[str]] = []
        real = runner.ServedEngine.__init__

        def spy(self: Any, *a: Any, **kw: Any) -> None:
            real(self, *a, **kw)
            seen.append(list(self.command))

        monkeypatch.setattr(runner.ServedEngine, "__init__", spy)
        from tests.unit.test_engine_sessions import Ops

        ops = Ops()
        ops.send(op="close")
        # On a card: an engine that reports the CPU would be started twice.
        for key, value in stub_env(SERVE_STUB_DEVICE="cuda").items():
            monkeypatch.setenv(key, value)
        monkeypatch.setitem(runner.SERVED_ENGINES, "mokuro", (STUB_MODULE, ()))
        monkeypatch.setattr(sys, "stdin", ops)
        from tests.unit.test_engine_sessions import Stdout

        stdout = Stdout()
        args = runner.parse_args(
            [
                "--serve",
                "--engine",
                "mokuro",
                # A forced fp16 on a card: the serve process's own --fp16.
                "--precision",
                "fp16",
                "--session-log",
                str(tmp_path / "session.log"),
                "--mokuro-python",
                sys.executable,
                "--stage-workers",
                "mokuro=3",
            ]
        )
        assert runner.serve(args, stdout=stdout) == 0
        assert len(seen) == 1, seen
        # The engine's own flags, then its own pipeline width. Nothing of
        # ours: a width here is not a second process.
        assert seen[0][1:] == ["-m", STUB_MODULE, "--fp16", "--num_workers", "3"], seen[0]

    # -- ADDENDUM 7 on this road: the Device cell reaches the process --------

    @classmethod
    def _spawned(
        cls,
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
        flags: Sequence[str] = (),
        **knobs: str,
    ) -> tuple[list[str], dict[str, str], dict[str, Any]]:
        """Open a session and report (argv, env, the ready event)."""
        spawns, ready = cls._spawns(tmp_path, monkeypatch, flags, **knobs)
        assert len(spawns) == 1, spawns
        command, env = spawns[0]
        return command, env, ready

    @staticmethod
    def _spawns(
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
        flags: Sequence[str] = (),
        **knobs: str,
    ) -> tuple[list[tuple[list[str], dict[str, str]]], dict[str, Any]]:
        """Open a session and report every process started, and the ready event.

        A process's env is what it ACTUALLY got: ``env=None`` inherits ours,
        so that case reads back as a copy of this process's environment.
        """
        seen: list[tuple[list[str], dict[str, str]]] = []
        real = runner.ServedEngine.__init__

        def spy(self: Any, *a: Any, **kw: Any) -> None:
            real(self, *a, **kw)
            seen.append(
                (list(self.command), dict(self._env if self._env is not None else os.environ))
            )

        monkeypatch.setattr(runner.ServedEngine, "__init__", spy)
        from tests.unit.test_engine_sessions import Ops, Stdout

        ops = Ops()
        ops.send(op="close")
        for key, value in stub_env(**knobs).items():
            monkeypatch.setenv(key, value)
        monkeypatch.setitem(runner.SERVED_ENGINES, "mokuro", (STUB_MODULE, ()))
        monkeypatch.setattr(sys, "stdin", ops)
        stdout = Stdout()
        args = runner.parse_args(
            [
                "--serve",
                "--engine",
                "mokuro",
                "--session-log",
                str(tmp_path / "session.log"),
                "--mokuro-python",
                sys.executable,
                *flags,
            ]
        )
        assert runner.serve(args, stdout=stdout) == 0
        return seen, stdout.events()[0]

    def test_mokuro_cpu_starts_the_process_with_force_cpu(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The MONOLITHIC bullet of ADDENDUM 7, through the pipe."""
        command, env, ready = self._spawned(
            tmp_path, monkeypatch, ["--stage-device", "mokuro=cpu"], SERVE_STUB_DEVICE="cpu"
        )
        assert command[-1] == "--force_cpu"
        assert "CUDA_VISIBLE_DEVICES" not in env
        assert ready["stage_device"] == {"mokuro": "cpu"}

    def test_mokuro_on_a_card_hides_the_others_from_the_process(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """mokuro has no device index, so the card is chosen by hiding the rest.

        BOTH variables, because which one a torch build reads depends on CUDA
        against ROCm -- and the row's index is reported back as asked, since
        the process itself can only ever see its card as number 0.
        """
        command, env, ready = self._spawned(
            tmp_path, monkeypatch, ["--stage-device", "mokuro=gpu:1"], SERVE_STUB_DEVICE="cuda"
        )
        assert "--force_cpu" not in command
        assert env["CUDA_VISIBLE_DEVICES"] == "1"
        assert env["HIP_VISIBLE_DEVICES"] == "1"
        # The rest of OUR environment travels with them: the two variables
        # alone would take the child's PATH and its venv away.
        assert env["PATH"] == os.environ["PATH"]
        assert ready["stage_device"] == {"mokuro": "gpu:1"}

    def test_auto_says_nothing_to_the_process_and_reports_what_it_got(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """``auto`` is today's behaviour: the fork picks, and then it is asked.

        The road is planned without a torch probe, so an unasked device shows
        as the CPU until the ready line answers -- but nothing of that guess
        may reach the process, or the guess would make itself true.
        """
        command, env, ready = self._spawned(tmp_path, monkeypatch, SERVE_STUB_DEVICE="cuda")
        assert command[1:] == ["-m", STUB_MODULE]
        assert "CUDA_VISIBLE_DEVICES" not in env and "HIP_VISIBLE_DEVICES" not in env
        assert ready["stage_device"] == {"mokuro": "gpu:0"}

        _, on_cpu = self._spawns(tmp_path, monkeypatch, SERVE_STUB_DEVICE="cpu")
        assert on_cpu["stage_device"] == {"mokuro": "cpu"}

    # -- torch's CPU pool in the served process ------------------------------
    #
    # Measured on tower (RTX 4090, 48 CPU threads; mokuro in fp16, served): beside
    # 24 busy-spinning processes 29.43 / 29.25 / 29.73 pages/s at torch's
    # default pool against 46.31 / 46.60 / 46.40 with OMP_NUM_THREADS=1;
    # beside 12, 46.9 against 47.3; idle 45.52 against 47.04.

    @staticmethod
    def _no_thread_env(monkeypatch: pytest.MonkeyPatch) -> None:
        for name in runner.TORCH_THREAD_ENV:
            monkeypatch.delenv(name, raising=False)

    def test_a_card_starts_the_engine_on_one_torch_thread(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        self._no_thread_env(monkeypatch)
        _, env, ready = self._spawned(
            tmp_path, monkeypatch, ["--stage-device", "mokuro=gpu:0"], SERVE_STUB_DEVICE="cuda"
        )
        assert env["OMP_NUM_THREADS"] == "1"
        # OMP alone, as measured: MKL takes its count from it when unset.
        assert "MKL_NUM_THREADS" not in env
        assert env["CUDA_VISIBLE_DEVICES"] == "0"
        assert ready["stage_device"] == {"mokuro": "gpu:0"}

    def test_the_cpu_keeps_torch_s_default_pool(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """On the CPU the pool IS the engine's compute."""
        self._no_thread_env(monkeypatch)
        command, env, _ = self._spawned(
            tmp_path, monkeypatch, ["--stage-device", "mokuro=cpu"], SERVE_STUB_DEVICE="cpu"
        )
        assert command[-1] == "--force_cpu"
        assert "OMP_NUM_THREADS" not in env and "MKL_NUM_THREADS" not in env

    def test_auto_that_lands_on_a_card_keeps_the_one_thread(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The live rows are ``auto``: the fork picks the card, one process."""
        self._no_thread_env(monkeypatch)
        command, env, ready = self._spawned(tmp_path, monkeypatch, SERVE_STUB_DEVICE="cuda")
        assert command[1:] == ["-m", STUB_MODULE]
        assert env["OMP_NUM_THREADS"] == "1"
        assert ready["stage_device"] == {"mokuro": "gpu:0"}

    def test_an_engine_that_lands_on_the_cpu_is_restarted_on_the_default_pool(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """``auto`` on a host with no card: the cap would starve the CPU engine.

        Where the fork put the model is only known from its ready line, so a
        capped process that reports the CPU is closed and started again
        without the cap, before any page is sent.
        """
        self._no_thread_env(monkeypatch)
        spawns, ready = self._spawns(tmp_path, monkeypatch, SERVE_STUB_DEVICE="cpu")
        assert len(spawns) == 2, spawns
        (first_cmd, first_env), (second_cmd, second_env) = spawns
        assert first_env["OMP_NUM_THREADS"] == "1"
        assert "OMP_NUM_THREADS" not in second_env and "MKL_NUM_THREADS" not in second_env
        assert first_cmd == second_cmd
        assert ready["stage_device"] == {"mokuro": "cpu"}

    @pytest.mark.parametrize("variable", ["OMP_NUM_THREADS", "MKL_NUM_THREADS"])
    def test_an_operator_s_thread_count_is_kept(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, variable: str
    ) -> None:
        self._no_thread_env(monkeypatch)
        monkeypatch.setenv(variable, "6")
        _, env, _ = self._spawned(
            tmp_path, monkeypatch, ["--stage-device", "mokuro=gpu:0"], SERVE_STUB_DEVICE="cuda"
        )
        assert env[variable] == "6"
        other = next(name for name in runner.TORCH_THREAD_ENV if name != variable)
        assert other not in env

    def test_a_dead_engine_ends_the_session_instead_of_blanking_the_volume(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from tests.unit.test_engine_sessions import Ops, Stdout, volume_op

        pages = ("001.webp", "002.webp", "003.webp", "004.webp")
        ops = Ops()
        ops.send(
            **volume_op(
                tmp_path,
                "one",
                archive=str(self._archive(tmp_path, "one", pages)),
                workspace=str(tmp_path / "ws"),
            )
        )
        ops.send(op="close")
        for key, value in stub_env(SERVE_STUB_FATAL_AFTER="2", SERVE_STUB_HOLD="4").items():
            monkeypatch.setenv(key, value)
        monkeypatch.setitem(runner.SERVED_ENGINES, "mokuro", (STUB_MODULE, ()))
        monkeypatch.setattr(sys, "stdin", ops)
        stdout = Stdout()
        args = runner.parse_args(
            [
                "--serve",
                "--engine",
                "mokuro",
                "--session-log",
                str(tmp_path / "session.log"),
                "--mokuro-python",
                sys.executable,
            ]
        )
        assert runner.serve(args, stdout=stdout) == 1
        events = stdout.events()
        assert [e["event"] for e in events if e["event"] == "fatal"], events
        assert not (tmp_path / "out" / "one" / "one.mokuro").exists()

    def test_a_page_the_engine_refuses_leaves_the_volume_the_right_length(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from tests.unit.test_engine_sessions import Ops, volume_op

        # The stub's "pages" are text, so the usual blank-page path cannot
        # read a size out of them; a real page can, and a sidecar one page
        # short of its volume is refused whole by every reader.
        monkeypatch.setattr(
            runner,
            "blank_page_bytes",
            lambda data: {
                "version": "0.0.stub",
                "img_width": 0,
                "img_height": 0,
                "blocks": [],
            },
        )
        pages = ("001.webp", "002.webp", "003.webp")
        ops = Ops()
        ops.send(
            **volume_op(
                tmp_path,
                "one",
                archive=str(self._archive(tmp_path, "one", pages)),
                workspace=str(tmp_path / "ws"),
            )
        )
        ops.send(op="close")
        events = self._serve(tmp_path, monkeypatch, ops, SERVE_STUB_FAIL="002.webp")
        done = next(e for e in events if e["event"] == "volume_done")
        assert done["failed_pages"] == 1
        sidecar = json.loads(
            (tmp_path / "out" / "one" / "one.mokuro").read_text(encoding="utf-8")
        )
        assert [p["img_path"] for p in sidecar["pages"]] == list(pages)
        assert sidecar["pages"][1]["blocks"] == []

    def test_nothing_of_the_spool_is_left_when_the_volume_is_done(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from tests.unit.test_engine_sessions import Ops, volume_op

        pages = tuple(f"{i:03}.webp" for i in range(10))
        workspace = tmp_path / "ws"
        ops = Ops()
        ops.send(
            **volume_op(
                tmp_path,
                "one",
                archive=str(self._archive(tmp_path, "one", pages)),
                workspace=str(workspace),
            )
        )
        ops.send(op="close")
        self._serve(tmp_path, monkeypatch, ops, SERVE_STUB_WINDOW="3")
        assert not list(workspace.rglob("*.webp"))


class TestTheWindowIsAFloorToo:
    """The engine may hold a whole window before it answers anything.

    mokuro forms its OCR batches from the crop sequence and the LAST partial
    batch waits for the end of the volume, so an engine given fewer pages
    than a batch answers nothing at all until it is told the volume is over.
    A queue in front of it shorter than its window therefore does not merely
    run slower -- both sides wait for each other. These pin the two places
    that got it wrong.
    """

    def _bench(self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, pages: int) -> list[dict]:
        from tests.unit.test_engine_sessions import Stdout

        sample = tmp_path / "sample"
        sample.mkdir()
        for i in range(pages):
            (sample / f"{i:03}.webp").write_text(f"{i:03}.webp", encoding="utf-8")
        for key, value in stub_env(
            # Answers nothing until it has 64 pages or is told the volume
            # ended: exactly the shape of a partial OCR batch.
            SERVE_STUB_WINDOW="64",
            SERVE_STUB_HOLD="64",
        ).items():
            monkeypatch.setenv(key, value)
        monkeypatch.setitem(runner.SERVED_ENGINES, "mokuro", (STUB_MODULE, ()))
        stdout = Stdout()
        args = runner.parse_args(
            [
                "--bench",
                "--engine",
                "mokuro",
                "--mokuro-python",
                sys.executable,
                "--input",
                str(sample),
                "--session-log",
                str(tmp_path / "bench.log"),
                "--bench-max-trials",
                "3",
                "--bench-budget-seconds",
                "60",
            ]
        )
        assert runner.bench(args, stdout=stdout) == 0
        return stdout.events()

    def test_a_benchmark_of_a_served_row_runs_its_trials_and_answers(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Every trial re-pools; none of them may lose the engine's window."""
        events = self._bench(tmp_path, monkeypatch, pages=12)
        ready = next(e for e in events if e["event"] == "bench_ready")
        assert ready["stage_keys"] == ["feed", "mokuro", "post"]
        trials = [e for e in events if e["event"] == "bench_trial"]
        assert trials, events
        for trial in trials:
            assert trial["queue_capacity"]["feed"] == 64
            assert trial["stage_workers"]["mokuro"] == 1
            assert trial["pages_per_second"] > 0
        done = next(e for e in events if e["event"] == "bench_done")
        assert done["best"]["pages_per_second"] > 0

    def test_the_queue_in_front_of_the_engine_is_the_window_whatever_is_asked(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from tests.unit.test_engine_sessions import Ops, Stdout

        for key, value in stub_env(SERVE_STUB_WINDOW="11").items():
            monkeypatch.setenv(key, value)
        monkeypatch.setitem(runner.SERVED_ENGINES, "mokuro", (STUB_MODULE, ()))
        ops = Ops()
        ops.send(op="close")
        monkeypatch.setattr(sys, "stdin", ops)
        stdout = Stdout()
        args = runner.parse_args(
            [
                "--serve",
                "--engine",
                "mokuro",
                "--session-log",
                str(tmp_path / "session.log"),
                "--mokuro-python",
                sys.executable,
                # Both of these would strand the engine below its window.
                "--queue-capacity",
                "feed=2",
                "--cpu-workers",
                "0",
            ]
        )
        assert runner.serve(args, stdout=stdout) == 0
        ready = stdout.events()[0]
        assert ready["queue_capacity"]["feed"] == 11
        # ... and no stage fell back to the serial path, which would put one
        # page at a time through an engine that answers in batches.
        assert min(ready["stage_workers"].values()) >= 1
        log = (tmp_path / "session.log").read_text(encoding="utf-8")
        assert "queue-capacity feed=2 ignored" in log


class TestTheProbeAndTheFallback:
    """A package without the serve module keeps the one-volume CLI."""

    @staticmethod
    def _processor(tmp_path: Path, python: Path) -> Any:
        from mokuro_bunko.ocr.processor import OCRProcessor

        return OCRProcessor(storage_path=tmp_path / "storage", python_path=python)

    @pytest.fixture(autouse=True)
    def _forget_the_probe(self) -> Any:
        from mokuro_bunko.ocr import processor as processor_module

        processor_module._SERVE_PROBE.clear()
        yield
        processor_module._SERVE_PROBE.clear()

    def test_the_probe_answers_yes_for_a_module_that_is_really_there(
        self, tmp_path: Path
    ) -> None:
        """``json`` stands in for ``mokuro.serve``: no mokuro env needed."""
        from mokuro_bunko.ocr import processor as processor_module

        said: list[str] = []
        assert processor_module.serve_module_available(sys.executable, "json", said.append)
        assert any("pages stream into one process" in line for line in said), said

    def test_a_package_with_the_module_streams_its_pages(self, tmp_path: Path) -> None:
        from mokuro_bunko.ocr import processor as processor_module
        from mokuro_bunko.ocr.generations import DEFAULT_GENERATION

        processor = self._processor(tmp_path, Path(sys.executable))
        # What the probe would have answered on a host whose mokuro env has
        # the module -- the interpreter running these tests has no mokuro.
        processor_module._SERVE_PROBE[(sys.executable, "mokuro.serve")] = True
        assert processor.serves_pages(DEFAULT_GENERATION)
        assert not processor.runs_mokuro_cli(DEFAULT_GENERATION)

    def test_a_package_without_it_keeps_the_command_line_path(self, tmp_path: Path) -> None:
        from mokuro_bunko.ocr.generations import DEFAULT_GENERATION

        said: list[str] = []
        processor = self._processor(tmp_path, Path(sys.executable))
        processor.status_callback = said.append
        assert not processor.serves_pages(DEFAULT_GENERATION)
        assert processor.runs_mokuro_cli(DEFAULT_GENERATION)
        assert any("mokuro.serve is not in" in line for line in said), said
        assert any("one-volume command-line path" in line for line in said), said

    def test_the_probe_is_asked_once(self, tmp_path: Path) -> None:
        from mokuro_bunko.ocr import processor as processor_module

        calls: list[list[str]] = []
        real = processor_module.subprocess.run

        def counted(cmd: Any, **kwargs: Any) -> Any:
            calls.append(list(cmd))
            return real(cmd, **kwargs)

        processor_module.subprocess.run = counted  # type: ignore[assignment]
        try:
            for _ in range(5):
                processor_module.serve_module_available(sys.executable, "json")
        finally:
            processor_module.subprocess.run = real  # type: ignore[assignment]
        assert len(calls) == 1, calls
        assert calls[0][1:] == ["-c", "import json"]

    def test_a_served_row_passes_the_runner_its_own_interpreter(self, tmp_path: Path) -> None:
        from mokuro_bunko.ocr.generations import GenerationSpec

        processor = self._processor(tmp_path, tmp_path / "mokuro-env" / "bin" / "python")
        processor.engines_python_path = tmp_path / "engines-env" / "bin" / "python"
        served = GenerationSpec(id="g-1", name="mokuro", engine="mokuro", primary=True)
        composed = GenerationSpec(id="g-2", name="hayai", engine="hayai-nova")
        cmd = processor.session_command(served, tmp_path / "session.log")
        assert "--mokuro-python" in cmd
        assert cmd[cmd.index("--mokuro-python") + 1] == str(processor.python_path)
        assert "--mokuro-python" not in processor.session_command(
            composed, tmp_path / "session.log"
        )


class TestTheRowInTheAdmin:
    """The mokuro row has a stage table now, and its Workers cell means something."""

    @staticmethod
    def _rows(engine: str) -> list[dict[str, Any]]:
        from mokuro_bunko.admin.api import _stage_rows
        from mokuro_bunko.ocr.generations import GenerationSpec

        row = GenerationSpec(id="g-1", name=engine, engine=engine, primary=True)
        return _stage_rows(row, budget=8, gpu=True)

    def test_a_mokuro_row_shows_its_three_stages(self) -> None:
        rows = self._rows("mokuro")
        assert [r["key"] for r in rows] == ["feed", "mokuro", "post"]
        assert [r["device"] for r in rows] == ["cpu", "gpu:0", "cpu"]

    def test_only_the_engine_stage_offers_a_device(self) -> None:
        """ADDENDUM 7's Device select, on ADDENDUM 8's shape.

        The serve process holds this road's one model, so it is the one cell
        that is a control -- offering the WHOLE catalog, because there is no
        reason mokuro cannot run on any card the host has (or on none).
        """
        from mokuro_bunko.admin.api import _stage_rows
        from mokuro_bunko.ocr.devices import DeviceCatalog, GpuDevice

        catalog = DeviceCatalog(gpus=(GpuDevice(0, "A"), GpuDevice(1, "B")), probed=True)
        from mokuro_bunko.ocr.generations import GenerationSpec

        row = GenerationSpec(id="g-1", name="mokuro", engine="mokuro", primary=True)
        rows = {r["key"]: r for r in _stage_rows(row, budget=8, gpu=True, devices=catalog)}
        assert rows["mokuro"]["devices_allowed"] == ["auto", "cpu", "gpu:0", "gpu:1"]
        assert rows["mokuro"]["device_locked_reason"] is None
        # feed spools a file and post assembles a dict: no model to place.
        assert rows["feed"]["devices_allowed"] == []
        assert rows["post"]["devices_allowed"] == []

    def test_the_engine_stage_s_workers_are_the_engine_s_own(self) -> None:
        rows = self._rows("mokuro")
        assert rows[1]["max_workers"] == 1
        assert rows[1]["workers_means"] == "engine"
        assert [r["workers_means"] for r in rows if r["key"] != "mokuro"] == ["pool", "pool"]

    def test_a_recognizer_on_a_card_counts_copies(self) -> None:
        """hayai-nova's engine stage on the card: the cell is copies of the model."""
        from mokuro_bunko.admin.api import _stage_rows
        from mokuro_bunko.ocr.generations import GenerationSpec

        row = GenerationSpec(id="g-2", name="h", engine="hayai-nova", detector="ppocr-manga")
        on_card = {r["key"]: r["workers_means"] for r in _stage_rows(row, budget=8, gpu=True)}
        assert on_card == {"detect": "pool", "engine": "copies", "post": "pool"}
        on_cpu = {r["key"]: r["workers_means"] for r in _stage_rows(row, budget=8, gpu=False)}
        assert on_cpu["engine"] == "pool"

    def test_a_composed_row_is_unchanged(self) -> None:
        rows = self._rows("ppocr-manga")
        assert [r["key"] for r in rows] == ["detect", "layout"]
        assert {r["workers_means"] for r in rows} == {"pool"}

    def test_a_mokuro_row_may_now_size_its_pools(self) -> None:
        from mokuro_bunko.ocr.generations import parse_generation_list

        rows = parse_generation_list(
            [
                {
                    "id": "g-1",
                    "name": "mokuro",
                    "engine": "mokuro",
                    "primary": True,
                    "pools": {"stage_workers": {"mokuro": 4, "feed": 2}},
                }
            ]
        )
        assert rows[0].pools.stage_workers == {"mokuro": 4, "feed": 2}
        assert rows[0].stage_keys == ("feed", "mokuro", "post")
        assert not rows[0].monolithic
        assert rows[0].served
        assert rows[0].mokuro_env
        assert rows[0].reported_detector is None
