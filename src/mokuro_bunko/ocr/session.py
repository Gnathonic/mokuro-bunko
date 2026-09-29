"""One runner process, held open, with whole volumes streaming through it.

A *session* is ``engine_runner.py --serve``: one subprocess that loads its
models ONCE and then reads volume after volume out of the archives the server
hands it. The pipeline inside it never drains at a volume boundary -- the
detect stage is already on volume N+1's pages while the engine finishes
volume N's -- which is the whole point: a model load costs ~10.5 s per
(volume, generation), and on a fast engine that is longer than reading the
volume.

What crosses the pipe, and nothing else:

* **stdin** carries ops, one JSON object a line:
  ``{"op": "volume", ...}`` (see :class:`SessionVolume`) and ``{"op": "close"}``.
* **stdout** carries protocol events, one JSON object a line: ``ready``,
  ``volume_started``, ``page``, ``volume_done``, ``volume_failed``, ``stats``
  and ``fatal``. Human-readable logging goes to FILES -- the session log and
  each volume's own log -- so a line of prose on stdout is a bug in the
  runner, not a message, and is dropped (counted, and logged at debug).
* **stderr** goes to a file beside the session log, so that a Python
  traceback from a runner that dies before it can emit ``fatal`` is still
  diagnosable without ever reaching the protocol stream.

This module owns the pipe and nothing above it. It does not claim jobs, does
not know what a generation is beyond carrying one, and never decides that a
volume succeeded: it turns a subprocess into a queue of events plus a
``submit``/``close``/``kill`` handle, and :mod:`mokuro_bunko.ocr.watcher`
does the scheduling. A reader thread drains stdout continuously so the pipe
can never fill and wedge the runner mid-volume.

Two events are the SESSION's own, not the runner's:

* ``{"event": "exit", "returncode": <int>}`` -- the process ended and stdout
  is at EOF. Always the last event of a session, exactly once.
* ``{"event": "spawn_failed", "error": "..."}`` -- the process could not be
  started at all, followed by an ``exit``, so that a caller's event loop is
  the only place that has to handle either.
"""

from __future__ import annotations

import json
import logging
import queue
import subprocess
import threading
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from mokuro_bunko.ocr.generations import GenerationSpec
from mokuro_bunko.ocr.staging import hold_staged_runner, release_staged_runner

logger = logging.getLogger(__name__)

# Events the runner is allowed to send. An object naming anything else is
# from a newer runner and is passed through to the caller, which ignores what
# it does not know -- forward compatibility costs nothing here, and a
# protocol reader that rejects unknown events would make every runner change
# a breaking one.
RUNNER_EVENTS: frozenset[str] = frozenset(
    {
        "ready",
        "volume_started",
        "page",
        "volume_done",
        "volume_failed",
        "stats",
        "fatal",
    }
)

# Bytes of the stderr file kept as a crash reason when the runner died
# without managing to emit a `fatal` event.
STDERR_TAIL_BYTES = 4000


@dataclass(frozen=True)
class SessionVolume:
    """One volume handed to an open session.

    The ARCHIVE is the source: the server does not extract anything, because
    the runner's feeder reads pages out of the ``.cbz`` one at a time just
    ahead of the pipeline and rolls from the last page of one archive into
    the first page of the next. What is in flight is bounded by the
    pipeline's own tickets, so neither memory nor disk grows with the volume
    or with the queue. ``input_dir`` is the other way in -- an
    already-extracted directory -- which the single-volume CLI, the tests and
    the benchmark sample use.

    Every path is decided HERE, at submit time, and never read back off the
    settings: ``output`` and ``log`` are fixed from the generation's name as
    it is now, so a rename lands on volumes submitted after it and cannot
    move a file a running volume is about to be collected under.
    """

    id: str
    workspace: Path
    output: Path
    cache_dir: Path
    detect_dir: Path
    log: Path
    title: str
    volume: str
    archive: Path | None = None
    input_dir: Path | None = None
    title_uuid: str | None = None
    volume_uuid: str | None = None
    # The archive's size in bytes when this op was built (the library's own
    # `stat`), or None. A REMOTE session sends it, so the processor can tell
    # a proxy serving the wrong bytes from the file, and the library compares
    # it with a fresh `stat` when a claim comes back (design section 6.2).
    archive_size: int | None = None
    # The library archive's stem, when ``archive`` is not named after it: a
    # remote processor hands its runner the verified download as
    # ``/proc/<pid>/fd/<n>``, and the thumbnail rule and the fallback title
    # are keyed on the ARCHIVE's name, never on that path's. Local sessions
    # leave it None; their archive is the library's own file.
    stem: str | None = None

    def to_op(self) -> dict[str, Any]:
        """The ``volume`` op exactly as it goes down the pipe."""
        op: dict[str, Any] = {
            "op": "volume",
            "id": self.id,
            "workspace": str(self.workspace),
            "output": str(self.output),
            "cache_dir": str(self.cache_dir),
            "detect_dir": str(self.detect_dir),
            "log": str(self.log),
            "title": self.title,
            "volume": self.volume,
            "title_uuid": self.title_uuid,
            "volume_uuid": self.volume_uuid,
        }
        if self.archive is not None:
            op["archive"] = str(self.archive)
            if self.stem is not None:
                op["stem"] = self.stem
        else:
            op["input"] = str(self.input_dir) if self.input_dir is not None else ""
        return op


class OcrSession:
    """A runner subprocess in ``--serve`` mode, and the events it sends.

    One session serves ONE generation snapshot: the engine, detector,
    patch budget and pool widths are fixed by the command line
    at open, and a settings change that alters any of them kills the session
    rather than trying to reconfigure it.
    """

    def __init__(
        self,
        generation: GenerationSpec,
        command: list[str],
        *,
        session_log: Path,
        env: Mapping[str, str] | None = None,
        popen_kwargs: Mapping[str, Any] | None = None,
    ) -> None:
        self.generation = generation
        self.command = list(command)
        self.session_log = session_log
        self.stderr_path = session_log.with_name(session_log.name + ".stderr")
        self._env = dict(env) if env is not None else None
        self._popen_kwargs = dict(popen_kwargs or {})
        self._process: subprocess.Popen[Any] | None = None
        self._reader: threading.Thread | None = None
        self._events: queue.Queue[dict[str, Any]] = queue.Queue()
        self._stderr_file: Any | None = None
        self._lock = threading.Lock()
        self._closing = False
        self._killed = False
        self._started = False
        self._held_runner: Path | None = None
        # Lines stdout carried that were not protocol. Counted rather than
        # fatal: a runner that prints a warning has a bug worth seeing in the
        # log, but the volumes in flight are not the thing to punish for it.
        self.garbage_lines = 0

    # --- lifecycle ------------------------------------------------------

    def start(self) -> bool:
        """Launch the runner. False (and a ``spawn_failed`` event) if it will not start."""
        with self._lock:
            if self._started:
                return self._process is not None
            self._started = True
            killed_first = self._killed
        if killed_first:
            # Killed before it ever started (a settings change or a pre-empt
            # that found nothing to kill yet): it never starts, and its exit
            # is reported like any other end.
            self._events.put({"event": "exit", "returncode": None})
            return False
        try:
            self.session_log.parent.mkdir(parents=True, exist_ok=True)
            self._stderr_file = self.stderr_path.open("w", encoding="utf-8", errors="replace")
            process = subprocess.Popen(  # noqa: S603 - argv built by the processor
                self.command,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=self._stderr_file,
                env=self._env,
                text=True,
                encoding="utf-8",
                errors="replace",
                bufsize=1,
                **self._popen_kwargs,
            )
        except (OSError, ValueError) as e:
            self._close_stderr()
            self._events.put({"event": "spawn_failed", "error": str(e)})
            self._events.put({"event": "exit", "returncode": None})
            return False
        with self._lock:
            self._process = process
            killed_meanwhile = self._killed
        if killed_meanwhile:
            # The kill landed while Popen ran, with no process to end yet.
            try:
                process.kill()
            except OSError:
                pass
        # The runner imports its detector adapters BY PATH, lazily, so the
        # staged build this process was started from must survive an upgrade
        # that stages a new one beside it (and the prune that follows).
        hold_staged_runner(Path(self.command[1]))
        self._held_runner = Path(self.command[1])
        self._reader = threading.Thread(
            target=self._read_stdout,
            name=f"ocr-session-{self.generation.name}",
            daemon=True,
        )
        self._reader.start()
        return True

    @property
    def pid(self) -> int | None:
        process = self._process
        return process.pid if process is not None else None

    @property
    def closing(self) -> bool:
        """True once ``close()`` has been sent: an exit is then expected."""
        return self._closing

    @property
    def killed(self) -> bool:
        return self._killed

    def is_alive(self) -> bool:
        process = self._process
        return process is not None and process.poll() is None

    # --- ops ------------------------------------------------------------

    def submit(self, volume: SessionVolume) -> bool:
        """Send one volume. False when the pipe is already gone."""
        return self._write(volume.to_op())

    def close(self) -> None:
        """Ask for a clean end: finish the accepted volumes, then exit 0.

        Both halves of the contract are sent -- the explicit ``close`` op and
        then EOF -- because a runner may be waiting on either, and a runner
        that acts on the op still has to see its stdin end to be sure nothing
        more is coming.
        """
        with self._lock:
            if self._closing:
                return
            self._closing = True
        self._write({"op": "close"})
        process = self._process
        if process is not None and process.stdin is not None:
            try:
                process.stdin.close()
            except OSError:
                pass

    def wait(self, timeout: float | None = None) -> int | None:
        """Wait for the process to exit; None when it is still running."""
        process = self._process
        if process is None:
            return None
        try:
            return process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            return None

    def kill(self) -> bool:
        """End the runner now. Safe from any thread and safe to repeat.

        Before `start` it means the runner never starts; while `start` is
        inside Popen, the new process is killed as soon as it exists.
        """
        with self._lock:
            self._killed = True
            process = self._process
        if process is None or process.poll() is not None:
            return False
        try:
            process.kill()
        except OSError:
            return False
        return True

    def join_reader(self, timeout: float = 5.0) -> None:
        reader = self._reader
        if reader is not None and reader.is_alive():
            reader.join(timeout=timeout)

    # --- events ---------------------------------------------------------

    def poll_event(self, timeout: float | None = None) -> dict[str, Any] | None:
        """The next event, or None when none arrived within ``timeout``."""
        try:
            return self._events.get(timeout=timeout)
        except queue.Empty:
            return None

    def stderr_tail(self) -> str | None:
        """The end of the runner's stderr, as a crash reason. None when empty."""
        try:
            data = self.stderr_path.read_bytes()
        except OSError:
            return None
        if not data:
            return None
        text = data[-STDERR_TAIL_BYTES:].decode("utf-8", errors="replace").strip()
        if not text:
            return None
        lines = [line.strip() for line in text.splitlines() if line.strip()]
        return lines[-1][:300] if lines else None

    # --- internals ------------------------------------------------------

    def _write(self, op: Mapping[str, Any]) -> bool:
        process = self._process
        if process is None or process.stdin is None or process.poll() is not None:
            return False
        try:
            process.stdin.write(json.dumps(op, ensure_ascii=False) + "\n")
            process.stdin.flush()
        except (OSError, ValueError):
            return False
        return True

    def _read_stdout(self) -> None:
        """Drain stdout for the life of the process, parsing protocol lines.

        Nothing here may raise: this thread is the only thing keeping the
        pipe empty, and a runner whose stdout buffer fills stops mid-volume
        with no event and no way to say so.
        """
        process = self._process
        assert process is not None
        stream = process.stdout
        try:
            if stream is not None:
                for line in stream:
                    self._consume(line)
        except (OSError, ValueError) as e:  # pragma: no cover - pipe torn down
            logger.debug("OCR session %s stdout ended: %s", self.generation.name, e)
        finally:
            code: int | None
            try:
                code = process.wait()
            except Exception:  # pragma: no cover - defensive
                code = None
            self._close_stderr()
            self._release_runner()
            self._events.put({"event": "exit", "returncode": code})

    def _consume(self, line: str) -> None:
        text = line.strip()
        if not text:
            return
        try:
            event = json.loads(text)
        except ValueError:
            self._note_garbage(text)
            return
        if not isinstance(event, dict) or not isinstance(event.get("event"), str):
            self._note_garbage(text)
            return
        self._events.put(event)

    def _note_garbage(self, text: str) -> None:
        self.garbage_lines += 1
        logger.debug(
            "OCR session %s wrote a non-protocol line to stdout: %s",
            self.generation.name,
            text[:200],
        )

    def _release_runner(self) -> None:
        held = self._held_runner
        self._held_runner = None
        if held is not None:
            release_staged_runner(held)

    def _close_stderr(self) -> None:
        handle = self._stderr_file
        self._stderr_file = None
        if handle is not None:
            try:
                handle.close()
            except OSError:  # pragma: no cover - defensive
                pass
