"""One runner build and one storage per processor process (design section 5.1).

A processor stages its runner ONCE, at start, and runs that build until it
exits. Before this, `OCRProcessor.session_command` restaged at every session
open, so new code placed under a running processor (both live processors run
editable installs) reached its next session as a new runner while its
already-imported bridge stayed old -- an old bridge talking to a new runner.

And one processor per storage: `stage_runner` prunes every staged build ITS
OWN process is not using, so a second processor on the same storage would
delete the first one's runner from under it.
"""

from __future__ import annotations

import logging
import sys
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr import staging
from mokuro_bunko.ocr.generations import GenerationSpec
from mokuro_bunko.ocr.processor import OCRProcessor


def _row() -> GenerationSpec:
    return GenerationSpec(
        id="g-1", name="hayai", engine="hayai-nova", primary=True, enabled=True
    )


def _changed_sources(original: Any) -> Any:
    """The runner's sources as they would read after an in-place update."""

    def sources() -> list[tuple[str, str]]:
        return [
            (name, text + "\n# updated in place\n") if name == "engine_runner.py" else (name, text)
            for name, text in original()
        ]

    return sources


class _Client:
    """Just enough of a LibraryClient for a bridge that is never connected."""

    def close(self) -> None:
        return None


class TestTheRunnerIsPinnedAtStart:
    def test_the_digest_is_the_hash_half_of_the_staged_directory(
        self, tmp_path: Path
    ) -> None:
        staged = staging.stage_runner(tmp_path)
        assert staged.parent.name == f"{staging.RUNNER_STAGE_PREFIX}{staging.runner_digest()}"

    def test_a_processor_given_a_staged_runner_never_restages(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        pinned = staging.pin_runner(tmp_path)
        try:
            monkeypatch.setattr(
                staging, "_runner_sources", _changed_sources(staging._runner_sources)
            )
            processor = OCRProcessor(
                storage_path=tmp_path,
                generations=[_row()],
                engines_python_path=Path(sys.executable),
                python_path=Path(sys.executable),
                staged_runner=pinned,
            )
            session = processor.session_command(_row(), tmp_path / "session.log")
            bench = processor.open_bench(_row(), tmp_path / "sample", tmp_path / "bench.log")
            assert session[1] == str(pinned)
            assert bench.command[1] == str(pinned)
            assert pinned.is_file(), "the pinned build is still there"
            staged = sorted(p.name for p in (tmp_path / ".processing").iterdir())
            assert staged == [pinned.parent.name], "nothing new was staged"
        finally:
            staging.release_staged_runner(pinned)

    def test_the_library_s_own_processor_still_restages(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Unchanged for the library: its local road stages per session."""
        processor = OCRProcessor(
            storage_path=tmp_path,
            generations=[_row()],
            engines_python_path=Path(sys.executable),
            python_path=Path(sys.executable),
        )
        first = processor.session_command(_row(), tmp_path / "session.log")[1]
        monkeypatch.setattr(
            staging, "_runner_sources", _changed_sources(staging._runner_sources)
        )
        second = processor.session_command(_row(), tmp_path / "session.log")[1]
        assert first != second
        assert Path(second).is_file()

    def test_a_bridge_runs_its_pinned_build_and_warns_once_when_the_code_moves(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
        caplog: pytest.LogCaptureFixture,
    ) -> None:
        from mokuro_bunko.processor import bridge as bridge_module
        from mokuro_bunko.processor.bridge import RunnerBridge

        monkeypatch.setattr(bridge_module, "_drift_warned", False)
        monkeypatch.delenv("MOKURO_PROCESSOR_RUNNER", raising=False)
        pinned = staging.pin_runner(tmp_path)
        try:
            bridge = RunnerBridge(
                _Client(),  # type: ignore[arg-type]
                storage=tmp_path,
                engines_python=Path(sys.executable),
                runner=pinned,
            )
            monkeypatch.setattr(
                staging, "_runner_sources", _changed_sources(staging._runner_sources)
            )
            with caplog.at_level(logging.WARNING, logger=bridge_module.__name__):
                first = bridge._processor_for(_row())
                second = bridge._processor_for(_row())
            assert first.session_command(_row(), tmp_path / "a.log")[1] == str(pinned)
            assert second.open_bench(
                _row(), tmp_path / "sample", tmp_path / "b.log"
            ).command[1] == str(pinned)
            drift = [r for r in caplog.records if "code on disk changed" in r.getMessage()]
            assert len(drift) == 1, [r.getMessage() for r in caplog.records]
            assert pinned.parent.name in drift[0].getMessage()
        finally:
            staging.release_staged_runner(pinned)

    def test_no_warning_while_the_code_is_unchanged(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
        caplog: pytest.LogCaptureFixture,
    ) -> None:
        from mokuro_bunko.processor import bridge as bridge_module
        from mokuro_bunko.processor.bridge import RunnerBridge

        monkeypatch.setattr(bridge_module, "_drift_warned", False)
        pinned = staging.pin_runner(tmp_path)
        try:
            bridge = RunnerBridge(
                _Client(),  # type: ignore[arg-type]
                storage=tmp_path,
                engines_python=Path(sys.executable),
                runner=pinned,
            )
            with caplog.at_level(logging.WARNING, logger=bridge_module.__name__):
                bridge._processor_for(_row())
            assert not [r for r in caplog.records if "code on disk changed" in r.getMessage()]
        finally:
            staging.release_staged_runner(pinned)


class TestOneProcessorPerStorage:
    def _config(self, tmp_path: Path, storage: Path) -> Path:
        path = tmp_path / "processor.yaml"
        path.write_text(
            "library:\n"
            "  url: http://127.0.0.1:9\n"
            "  username: tower\n"
            "  password: hunter2hunter2\n"
            "processor:\n"
            f"  storage: {storage}\n",
            encoding="utf-8",
        )
        return path

    @pytest.mark.skipif(sys.platform == "win32", reason="no flock on Windows")
    def test_a_second_serve_on_the_same_storage_exits_naming_it(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from click.testing import CliRunner

        from mokuro_bunko.processor.cli import lock_storage, processor_group

        storage = tmp_path / "state"
        held = lock_storage(storage)
        assert held is not None
        try:
            monkeypatch.setenv("MOKURO_PROCESSOR_ENGINES_PYTHON", sys.executable)
            result = CliRunner().invoke(
                processor_group, ["serve", "--config", str(self._config(tmp_path, storage))]
            )
        finally:
            held.close()
        assert result.exit_code == 1, result.output
        assert "another processor is running on" in result.output
        assert str(storage) in result.output

    @pytest.mark.skipif(sys.platform == "win32", reason="no flock on Windows")
    def test_a_second_serve_never_prunes_the_first_one_s_runner(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The prune hazard, pinned: the live processor holds the lock and
        its runner; the code on disk moves; a second `serve` pointed at the
        same storage by mistake is refused BEFORE it stages anything, so the
        live build survives."""
        from click.testing import CliRunner

        from mokuro_bunko.processor.cli import lock_storage, processor_group

        storage = tmp_path / "state"
        held = lock_storage(storage)
        assert held is not None
        live = staging.stage_runner(storage)
        # The other "process": its own in-use table knows nothing of ours.
        monkeypatch.setattr(staging, "_staged_in_use", {})
        monkeypatch.setattr(
            staging, "_runner_sources", _changed_sources(staging._runner_sources)
        )
        monkeypatch.setenv("MOKURO_PROCESSOR_ENGINES_PYTHON", sys.executable)
        try:
            result = CliRunner().invoke(
                processor_group, ["serve", "--config", str(self._config(tmp_path, storage))]
            )
        finally:
            held.close()
        assert result.exit_code == 1, result.output
        assert live.is_file(), "the live processor's runner was pruned"
        staged = sorted(p.name for p in (storage / ".processing").iterdir() if p.is_dir())
        assert staged == [live.parent.name], "the refused serve staged a build"

    @pytest.mark.skipif(sys.platform == "win32", reason="no flock on Windows")
    def test_the_lock_is_released_with_its_holder(self, tmp_path: Path) -> None:
        from mokuro_bunko.processor.cli import lock_storage

        storage = tmp_path / "state"
        first = lock_storage(storage)
        assert first is not None
        assert lock_storage(storage) is None, "a second holder while the first lives"
        first.close()
        again = lock_storage(storage)
        assert again is not None
        again.close()
