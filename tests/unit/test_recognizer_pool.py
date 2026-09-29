"""Several copies of a GPU recognizer in one session (``--stage-workers engine=N``).

One copy of hayai-nova leaves most of a card idle; N copies, each in an engine
process of its own and driven by its own engine worker, fill the gaps. These
tests pin the pool's contract -- copies load one at a time and join as they
land, a call never shares a copy with another thread, a first copy that fails
fails the run, a copy whose process dies is dropped -- the engine process's
pipe protocol, and the session's rule: only an explicit ``engine=N`` on a card
starts engine processes.
"""

from __future__ import annotations

import os
import threading
import time
from typing import Any

import pytest

from mokuro_bunko.ocr import engine_runner as runner
from tests.fixtures import fake_engine_recognizer as fakes
from tests.unit.test_engine_sessions import fake_ppocr  # noqa: F401 - fixture


class _Copy:
    """A stand-in recognizer that notices being entered by two threads at once."""

    repos = {"some/recognizer": "a" * 40}

    def __init__(self, name: str, hold: float = 0.0) -> None:
        self.name = name
        self.hold = hold
        self.inside = 0
        self.overlapped = False
        self.calls = 0
        self._lock = threading.Lock()

    def __call__(self, crops: list[Any], max_tokens: Any = None) -> list[str]:
        with self._lock:
            self.inside += 1
            self.overlapped |= self.inside > 1
            self.calls += 1
        time.sleep(self.hold)
        with self._lock:
            self.inside -= 1
        return [self.name] * len(crops)


class TestThePool:
    def test_copies_load_one_after_another_and_join_as_they_land(self) -> None:
        gates = [threading.Event() for _ in range(3)]
        loading: list[int] = []
        made: list[_Copy] = []

        def load() -> Any:
            index = len(loading)
            loading.append(index)
            # a second load never starts before the first has finished
            assert loading == list(range(index + 1))
            gates[index].wait(5.0)
            made.append(_Copy(f"c{index}"))
            return made[-1]

        pool = runner.RecognizerPool("hayai-nova", load, 3)
        assert not pool.loaded and pool.token_caps is False
        gates[0].set()
        # the first page waits for ONE load, not three
        assert pool(["x"]) == ["c0"]
        assert pool.loaded and pool.weights() == {"some/recognizer": "a" * 40}
        gates[1].set()
        gates[2].set()
        pool._done.wait(5.0)
        assert [m.name for m in pool.members] == ["c0", "c1", "c2"]

    def test_no_copy_is_entered_by_two_threads_at_once(self) -> None:
        copies = iter([_Copy("a", hold=0.02), _Copy("b", hold=0.02)])
        pool = runner.RecognizerPool("paddle-manga", lambda: next(copies), 2)
        pool._done.wait(5.0)
        errors: list[BaseException] = []

        def work() -> None:
            try:
                for _ in range(10):
                    pool(["x"], max_tokens=[4])
            except BaseException as e:  # noqa: BLE001
                errors.append(e)

        threads = [threading.Thread(target=work) for _ in range(4)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(10.0)
        assert not errors
        members = pool.members
        assert sum(m.calls for m in members) == 40
        assert all(m.calls > 0 for m in members)  # both copies did work
        assert not any(m.overlapped for m in members)

    def test_a_first_copy_that_fails_fails_every_call_with_the_real_error(self) -> None:
        def boom() -> Any:
            raise RuntimeError("no such model")

        pool = runner.RecognizerPool("hayai-nova", boom, 2)
        for _ in range(2):  # the waker is put back for the next caller
            with pytest.raises(RuntimeError, match="no such model"):
                pool(["x"])
        assert isinstance(pool.error, RuntimeError)

    def test_a_later_copy_that_fails_leaves_the_pool_running_on_fewer(
        self, capsys: pytest.CaptureFixture[str]
    ) -> None:
        loads = iter([_Copy("a"), RuntimeError("out of memory")])

        def load() -> Any:
            got = next(loads)
            if isinstance(got, BaseException):
                raise got
            return got

        pool = runner.RecognizerPool("hayai-nova", load, 3)
        pool._done.wait(5.0)
        assert pool.error is None and [m.name for m in pool.members] == ["a"]
        assert pool(["x"]) == ["a"]
        said = capsys.readouterr()
        assert "copy 2 did not load (out of memory); running 1" in said.out + said.err


class TestTheEngineProcess:
    """A copy in a process of its own: real ``spawn`` children, stand-in models."""

    def test_it_reads_in_another_process_and_passes_token_caps_through(self) -> None:
        proc = runner.EngineProcess("hayai-nova", {}, load=fakes.load_echo)
        try:
            assert proc.pid != os.getpid()
            assert proc.repos == {"fake/recognizer": "b" * 40}
            assert proc(["a", "b"]) == [f"a@{proc.pid}", f"b@{proc.pid}"]
            assert proc(["c"], max_tokens=[7]) == [f"c@{proc.pid}/7"]
        finally:
            proc.close()
        assert not proc._process.is_alive()

    def test_a_load_that_fails_raises_the_real_error(self) -> None:
        with pytest.raises(RuntimeError, match="no such model"):
            runner.EngineProcess("hayai-nova", {}, load=fakes.load_boom)

    def test_a_bad_batch_raises_its_error_and_the_copy_keeps_serving(self) -> None:
        proc = runner.EngineProcess("hayai-nova", {}, load=fakes.load_echo)
        try:
            with pytest.raises(ValueError, match="cannot read"):
                proc(["boom"])
            assert proc(["ok"]) == [f"ok@{proc.pid}"]
        finally:
            proc.close()

    def test_a_process_that_dies_is_gone_not_a_bad_page(self) -> None:
        proc = runner.EngineProcess("hayai-nova", {}, load=fakes.load_echo)
        try:
            with pytest.raises(runner.EngineProcessGone, match="exit"):
                proc(["die"])
        finally:
            proc.close()

    def test_a_pool_drops_a_dead_copy_and_ends_with_the_last(self) -> None:
        count = iter(range(2))
        pool = runner.RecognizerPool(
            "hayai-nova",
            lambda: runner.EngineProcess("hayai-nova", {}, next(count), load=fakes.load_echo),
            2,
        )
        try:
            pool._done.wait(60.0)
            assert len(pool.members) == 2
            with pytest.raises(runner.EngineProcessGone):
                pool(["die"])
            assert pool.error is None
            assert pool(["x"])[0].startswith("x@")  # the survivor serves
            with pytest.raises(runner.EngineProcessGone):
                pool(["die"])
            with pytest.raises(runner.EngineProcessGone):  # the run's error now
                pool(["x"])
            assert isinstance(pool.error, runner.EngineProcessGone)
        finally:
            pool.close()


class _FakeProcess(_Copy):
    started: list[_FakeProcess] = []

    def __init__(self, engine: str, load_kwargs: dict[str, Any], index: int = 0) -> None:
        super().__init__(f"p{index}")
        self.load_kwargs = load_kwargs
        self.closed = False
        _FakeProcess.started.append(self)

    def close(self) -> None:
        self.closed = True


class TestTheSession:
    """Only an explicit ``engine=N`` on a card starts engine processes."""

    def _open(self, monkeypatch: pytest.MonkeyPatch, **config: Any) -> Any:
        monkeypatch.setattr(runner, "load_recognizer", lambda *a, **k: _Copy("r"))
        monkeypatch.setattr(runner, "EngineProcess", _FakeProcess)
        # The real line crop imports cv2, which lives only in the engines venv;
        # these sessions never crop a page.
        monkeypatch.setattr(runner, "make_line_crop_fn", lambda *a, **k: lambda *_: [])
        _FakeProcess.started = []
        return runner.OpenPipeline(
            runner.SessionConfig(engine="hayai-nova", detector="ppocr-manga", **config)
        )

    @pytest.mark.usefixtures("fake_ppocr")
    def test_engine_n_on_a_card_is_n_processes_and_n_workers(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        session = self._open(monkeypatch, stage_workers="engine=3", stage_device="engine=gpu:0")
        try:
            assert session.engine_copies == 3
            assert isinstance(session.loader, runner.RecognizerPool)
            engine = [
                w
                for s, w in zip(session.specs, session.widths, strict=True)
                if s.key == runner.STAGE_ENGINE
            ]
            assert engine == [3]
            session.loader._done.wait(5.0)
            assert len(_FakeProcess.started) == 3
            # each child loads the recognizer the session would have loaded
            assert _FakeProcess.started[0].load_kwargs == {
                "fold": False,
                "patches": runner.DEFAULT_PATCH_BUDGET,
                "device": "gpu:0",
                "precision": "auto-accuracy",
            }
        finally:
            session.pipeline.close()
            session.close()
        assert all(p.closed for p in _FakeProcess.started)

    @pytest.mark.usefixtures("fake_ppocr")
    def test_one_copy_unless_asked(self, monkeypatch: pytest.MonkeyPatch) -> None:
        session = self._open(monkeypatch, stage_device="engine=gpu:0")
        try:
            assert session.engine_copies == 1
            assert isinstance(session.loader, runner.DeferredRecognizer)
            assert _FakeProcess.started == []
        finally:
            session.pipeline.close()
            session.close()

    @pytest.mark.usefixtures("fake_ppocr")
    def test_no_copies_on_the_cpu(self, monkeypatch: pytest.MonkeyPatch) -> None:
        session = self._open(monkeypatch, stage_workers="engine=3", stage_device="engine=cpu")
        try:
            assert session.engine_copies == 1
            assert isinstance(session.loader, runner.DeferredRecognizer)
        finally:
            session.pipeline.close()
            session.close()
