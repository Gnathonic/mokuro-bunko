"""Unit tests for filesystem watcher middleware."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import Mock

from mokuro_bunko.middleware.fs_watcher import LibraryWatcher, _is_relevant


def test_is_relevant_matches_expected_suffixes() -> None:
    assert _is_relevant("library/book.cbz", is_directory=False) is True
    assert _is_relevant("library/book.mokuro", is_directory=False) is True
    assert _is_relevant("library/book.mokuro.gz", is_directory=False) is True
    assert _is_relevant("library/thumb.webp", is_directory=False) is True
    assert _is_relevant("library/notes.txt", is_directory=False) is False
    assert _is_relevant("library/dir", is_directory=True) is True


def test_start_is_noop_when_observer_already_running(monkeypatch, tmp_path: Path) -> None:
    watcher = LibraryWatcher(watch_path=tmp_path / "library", on_change=lambda: None)
    existing = Mock()
    watcher._observer = existing  # type: ignore[attr-defined]

    observer_ctor = Mock()
    monkeypatch.setattr("mokuro_bunko.middleware.fs_watcher.Observer", observer_ctor)

    watcher.start()

    observer_ctor.assert_not_called()
    assert watcher._observer is existing

def test_stop_stops_observer_and_clears_reference(monkeypatch) -> None:
    watcher = LibraryWatcher(watch_path=Path("library"), on_change=lambda: None)
    observer = Mock()
    watcher._observer = observer  # type: ignore[attr-defined]

    monkeypatch.setattr("mokuro_bunko.middleware.fs_watcher.sys.is_finalizing", lambda: False)

    watcher.stop()

    observer.stop.assert_called_once_with()
    observer.join.assert_called_once_with(timeout=5.0)
    assert watcher._observer is None


def test_stop_skips_observer_shutdown_during_finalization(monkeypatch) -> None:
    watcher = LibraryWatcher(watch_path=Path("library"), on_change=lambda: None)
    observer = Mock()
    watcher._observer = observer  # type: ignore[attr-defined]

    monkeypatch.setattr("mokuro_bunko.middleware.fs_watcher.sys.is_finalizing", lambda: True)

    watcher.stop()

    observer.stop.assert_not_called()
    observer.join.assert_not_called()
    assert watcher._observer is None

def test_stop_skips_observer_shutdown_when_requested() -> None:
    watcher = LibraryWatcher(watch_path=Path("library"), on_change=lambda: None)
    observer = Mock()
    watcher._observer = observer  # type: ignore[attr-defined]

    watcher.stop(skip_observer_shutdown=True)

    observer.stop.assert_not_called()
    observer.join.assert_not_called()
    assert watcher._observer is None
