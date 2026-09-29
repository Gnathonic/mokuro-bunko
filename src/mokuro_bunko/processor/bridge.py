"""Ops in, runner sessions out, events back.

One :class:`RunnerBridge` per connected processor process. It holds one
ordinary :class:`~mokuro_bunko.ocr.session.OcrSession` per ``open_session``
op -- the same subprocess, the same command line and the same environment
the library server would have built -- and does the two things the library
server cannot do from here: fetch the archive, and hand the finished file
back.

An archive is fetched WHOLE before its runner sees it (design sections 4-5):
downloaded at full speed into the spool (RAM, else this processor's
storage), resumed where a broken download stopped, verified against the
zip's own CRCs, and only then handed to the runner as an ordinary ARCHIVE --
the road the library itself runs locally, one reader, no per-page spool.
Nothing the runner does can pause a read of the library's socket any more,
which is what used to make the library's 10 s write timeout cut archives
short. The volume the runner is reading and the one on deck are the most a
session ever holds.

The processor never fails a volume. It delivers the archive -- and says
``fetch {state: ready}`` -- or gives the claim back with a class
(``volume_returned``), and the library judges that from its own file. What
an archive's CONTENT means is the runner's to decide, exactly as locally.

This module imports the library-side OCR driver, so it is imported LAZILY by
the CLI: an ordinary ``mokuro-bunko serve`` must not pay for it.
"""

from __future__ import annotations

import logging
import os
import queue
import re
import shutil
import threading
import zipfile
from collections.abc import Mapping
from pathlib import Path, PurePosixPath
from typing import Any

from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.processor import OCRProcessor
from mokuro_bunko.ocr.session import OcrSession, SessionVolume
from mokuro_bunko.ocr.staging import RUNNER_STAGE_PREFIX, pin_runner, runner_digest
from mokuro_bunko.processor.archives import (
    ArchiveFetcher,
    ArchiveSpool,
    FetchCancelled,
    FetchedArchive,
    FetchTiming,
    TransferFault,
    describe_damaged,
)
from mokuro_bunko.processor.client import (
    ACTION_REREGISTER,
    EventSink,
    LibraryClient,
    LibraryTransportError,
)

logger = logging.getLogger(__name__)

# Whether this PROCESS has already said that the code on disk moved on under
# its pinned runner. Once per process, not per registration: a bridge is
# rebuilt at every re-registration, and the fact has not changed.
_drift_warned = False


# What the library is told, appended to the runner's own failure, about a
# volume whose archive was proven damaged at the library before it was sent.
DAMAGED_AT_LIBRARY_NOTE = " (the library's copy: the same bytes on two downloads)"


def damaged_at_library_note(fetched: FetchedArchive) -> str:
    """The note for this archive: which members the processor proved damaged.

    The runner's own words say why the VOLUME failed ("every page failed",
    a `BadZipFile`), and the member it met first at best; the note adds
    which members fail their CRC-32 check, so the failure record names the
    damage precisely. A zip that will not open has no member to name: the
    runner's error already says what is wrong with it.
    """
    if not fetched.damaged:
        return DAMAGED_AT_LIBRARY_NOTE
    return f"{DAMAGED_AT_LIBRARY_NOTE[:-1]}; {describe_damaged(fetched.damaged)})"


def _as_int(value: Any) -> int | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return int(value)


class _Bridged:
    """One open runner session and the events body it reports through."""

    def __init__(
        self,
        sid: str,
        generation: GenerationSpec,
        processor: OCRProcessor,
        session: OcrSession,
        sink: EventSink,
    ) -> None:
        self.sid = sid
        self.generation = generation
        self.processor = processor
        self.session = session
        self.sink = sink
        self.volumes: dict[str, SessionVolume] = {}
        # The verified archive each claim's runner is reading, held until
        # that claim's terminal event -- and released BEFORE it is forwarded,
        # so the library's top-up can never find three archives held here.
        self.archives: dict[str, FetchedArchive] = {}
        # Claims whose archive was proven damaged at the library (two
        # downloads, the same failing bytes): delivered as they are, and the
        # runner's failure for one says so, naming the damaged members.
        self.damaged_at_library: dict[str, str] = {}
        self.cancelled: set[str] = set()
        self.lock = threading.Lock()
        self.pump: threading.Thread | None = None
        # The volumes this session has been sent, in ARRIVAL order. One
        # feeder fetches and submits them in turn, so the claims delivered to
        # the runner are always a prefix of the library's order -- which is
        # what its blame rule relies on (only a delivered claim is blamed).
        self.work: queue.Queue[Mapping[str, Any] | None] = queue.Queue()
        self.feeder: threading.Thread | None = None
        # Set when this session must take no more volumes: it was cancelled,
        # it was closed, or the library ended its body. Also the fetcher's
        # cancel: a download under way ends with it.
        self.stopping = threading.Event()
        # Set when this PROCESSOR is leaving (`RunnerBridge.shutdown`). From
        # then on nothing more is said on this session's events body: see
        # `RunnerBridge._report`.
        self.leaving = threading.Event()


# What a library mints for a session, a claim or a benchmark:
# `secrets.token_hex` and `v<n>`. The ids name files here
# (`logs/session.<sid>.log`), so one carrying anything else -- a separator, a
# `..` -- is a library to distrust, and its op is dropped whole.
_OP_ID = re.compile(r"[A-Za-z0-9_-]{1,64}")
_OP_ID_KEYS = ("sid", "claim", "bid")


class RunnerBridge:
    """Every op the library can send, and what this machine does about it."""

    def __init__(
        self,
        client: LibraryClient,
        *,
        storage: Path,
        engines_python: Path | None,
        concurrency: int = 1,
        runner: Path | None = None,
        spool: ArchiveSpool | None = None,
        timing: FetchTiming | None = None,
    ) -> None:
        self.client = client
        self.storage = Path(storage)
        self.engines_python = engines_python
        self.concurrency = concurrency
        # The runner build this processor runs, pinned once for the life of
        # the process (`processor serve` passes the one it staged at start).
        # A bridge built without one pins its own -- a test, or a caller
        # that is itself the whole process.
        self.runner = Path(runner) if runner is not None else pin_runner(self.storage)
        # Where archives are held, shared by every session of this process
        # (`processor serve` builds ONE, whose RAM budget is the processor's);
        # and what fetches them.
        self.spool = spool if spool is not None else ArchiveSpool(self.storage)
        self.fetcher = ArchiveFetcher(client, self.spool, timing=timing)
        self._sessions: dict[str, _Bridged] = {}
        self._benches: dict[str, threading.Event] = {}
        self._lock = threading.Lock()
        # Set once this processor is leaving -- `shutdown`, or the library
        # found unreachable mid-volume. From then on nothing more is said on
        # any body, benchmark bodies included.
        self._leaving = threading.Event()

    # -- ops ---------------------------------------------------------------

    def handle(self, op: Mapping[str, Any]) -> None:
        """One op. Whatever goes wrong handling it costs that op only.

        An exception here would otherwise leave `serve`'s op loop and end
        the whole processor -- every session with it -- for one bad op or a
        network blip on one channel.
        """
        kind = op.get("op")
        if kind == "heartbeat":
            return
        for key in _OP_ID_KEYS:
            if key in op and not _OP_ID.fullmatch(str(op[key])):
                logger.error("dropping a %r op: its %s %r is not an id", kind, key, op[key])
                return
        try:
            if kind == "open_session":
                self._open_session(op)
            elif kind == "volume":
                self._volume(op)
            elif kind == "cancel":
                self._cancel(op)
            elif kind == "close_session":
                self._close_session(op)
            elif kind == "bench":
                self._bench(op)
            else:
                logger.warning("unknown op %r from the library", kind)
        except Exception:  # noqa: BLE001 - one op, not the processor
            logger.exception("handling a %r op failed", kind)

    def _open_session(self, op: Mapping[str, Any]) -> None:
        sid = str(op.get("sid") or "")
        spec = op.get("generation")
        if not sid or not isinstance(spec, dict):
            logger.error("open_session without a sid and a generation")
            return
        if self._leaving.is_set():
            # This processor is on its way out; its registration is void.
            return
        try:
            sink = self.client.open_events(sid)
        except OSError as e:
            # Nothing can be reported without a body, and a runner nobody
            # can hear from is worse than no runner. The library cannot tell
            # a processor that could not reach it from one that is gone, and
            # treats a body that never opens as the processor leaving (claims
            # back, nothing blamed) -- so leave, and register again: a
            # library that is really there answers the registration.
            logger.error(
                "could not open the events body for session %s: %s; registering again",
                sid, e,
            )
            self.client.close()
            return
        if sink.ended:
            # Refused before a single event: there is nobody to report to,
            # so no runner is spawned -- only the refusal is acted on.
            self._react(sid, sink)
            return
        try:
            row = self._row(spec)
            processor = self._processor_for(row)
            log_dir = self.storage / "logs"
            log_dir.mkdir(parents=True, exist_ok=True)
            session = processor.open_session(row, log_dir / f"session.{sid}.log")
            override = os.environ.get("MOKURO_PROCESSOR_RUNNER")
            if override:
                # Run a different runner SCRIPT than the staged one -- the
                # test suite's fake runner, or a build being debugged --
                # while everything else about the command line, the
                # environment and the OS priority stays exactly what this
                # machine would really have used. `command[1]` is the script
                # (`OCRProcessor.session_command`); `command[0]` is the
                # interpreter, which is not this variable's to move.
                session.command[1] = override
        except (FileNotFoundError, OSError, ValueError) as e:
            sink.send({"event": "spawn_failed", "error": str(e)})
            sink.send({"event": "exit", "returncode": None})
            sink.close()
            return
        bridged = _Bridged(sid, row, processor, session, sink)
        with self._lock:
            self._sessions[sid] = bridged
        if not session.start():
            event = session.poll_event(timeout=5.0) or {}
            sink.send({"event": "spawn_failed", "error": str(event.get("error") or "")})
            sink.send({"event": "exit", "returncode": None})
            self._finish(bridged)
            return
        bridged.feeder = threading.Thread(
            target=self._feed, args=(bridged,), name=f"feed-{sid}", daemon=True
        )
        bridged.feeder.start()
        bridged.pump = threading.Thread(
            target=self._pump, args=(bridged,), name=f"bridge-{sid}", daemon=True
        )
        bridged.pump.start()
        # From here on, an end the library imposes is acted on by whichever
        # thread finds it -- the ping thread included, while the runner is
        # idle and no event of ours will ever touch the body again. The
        # library sends no op for a session it ended itself, so without this
        # an idle runner would keep its models loaded on a card the library
        # already counts as free. Attached only once the runner is running:
        # an end found earlier fires it at once, and a kill before the spawn
        # would be a kill of nothing.
        sink.on_end(lambda ended: self._on_sink_end(bridged, ended))

    def _processor_for(self, row: GenerationSpec) -> OCRProcessor:
        """The OCR driver for one session or benchmark, on the pinned build.

        Every command it builds names :attr:`runner` and nothing else, so no
        session of this process ever runs a runner newer than the bridge
        driving it. When the code on disk has moved on since the pin, that is
        said ONCE per process, at WARNING: the processor keeps its build
        until it is restarted, which is exactly what the deploy order (stop,
        update, start) relies on.
        """
        self._check_runner_drift()
        return OCRProcessor(
            storage_path=self.storage,
            generations=[row],
            engines_python_path=self.engines_python,
            concurrency=self.concurrency,
            staged_runner=self.runner,
        )

    def _check_runner_drift(self) -> None:
        global _drift_warned
        if _drift_warned:
            return
        try:
            current = f"{RUNNER_STAGE_PREFIX}{runner_digest()}"
        except OSError as e:  # pragma: no cover - a source file that vanished
            logger.debug("could not hash the runner's sources: %s", e)
            return
        pinned = self.runner.parent.name
        if current != pinned:
            _drift_warned = True
            logger.warning(
                "the code on disk changed since this processor started; it keeps "
                "running %s until it is restarted (the code on disk is %s)",
                pinned, current,
            )

    @staticmethod
    def _row(spec: Mapping[str, Any]) -> GenerationSpec:
        """The library's row dict, as a GenerationSpec this machine can run.

        `primary`/`enabled` are forced because a one-row list has to satisfy
        the same validation a config does; neither reaches the wire. The
        sidecar's NAME never comes from here -- it arrives on the volume op,
        so a rename on the library cannot move a file this machine is about
        to write.
        """
        row = dict(spec)
        row.update(primary=True, enabled=True)
        row.setdefault("name", "remote")
        return parse_generation_list([row])[0]

    def _volume(self, op: Mapping[str, Any]) -> None:
        sid, claim = str(op.get("sid") or ""), str(op.get("claim") or "")
        with self._lock:
            bridged = self._sessions.get(sid)
        if bridged is None:
            logger.warning("volume %r for an unknown session %r", claim, sid)
            return
        bridged.work.put(op)

    def _cancel(self, op: Mapping[str, Any]) -> None:
        """Stop a volume, or a benchmark. Nothing is recorded either way.

        ONE op, two payloads (Global Constraints): ``{sid, claim}`` for a
        claimed volume, ``{bid}`` for a running benchmark. A session holds a
        pipeline, not a volume, so there is no way to un-submit one page's
        worth of work; the honest cancel is the one the library already
        uses locally -- end the process, return the claims, record nothing.
        """
        bid = str(op.get("bid") or "")
        if bid:
            with self._lock:
                stop = self._benches.get(bid)
            if stop is not None:
                stop.set()
                self.fetcher.abort(stop)
            return
        sid, claim = str(op.get("sid") or ""), str(op.get("claim") or "")
        with self._lock:
            bridged = self._sessions.get(sid)
        if bridged is None:
            return
        with bridged.lock:
            bridged.cancelled.add(claim)
        bridged.stopping.set()
        self.fetcher.abort(bridged.stopping)
        bridged.session.kill()

    def _close_session(self, op: Mapping[str, Any]) -> None:
        """The library closes this session: finish what is in the runner.

        The runner finishes every archive it has already accepted -- closing
        puts the end of its feed BEHIND them, exactly as a local session
        closes -- and a claim still downloading is abandoned without a word:
        the library asked for the close (a shutdown, or a session it is
        ending) and settles that claim itself.
        """
        with self._lock:
            bridged = self._sessions.get(str(op.get("sid") or ""))
        if bridged is not None:
            bridged.stopping.set()
            self.fetcher.abort(bridged.stopping)
            bridged.session.close()

    def _bench(self, op: Mapping[str, Any]) -> None:
        """Run ``--bench`` for a spec on the sample the library packed.

        On a thread of its own: a benchmark runs for minutes, and the op
        loop that called this must keep reading the stream -- heartbeats,
        and the `cancel {bid}` that may end it.
        """
        bid = str(op.get("bid") or "")
        if not bid:
            logger.error("a bench op without a bid")
            return
        stop = threading.Event()
        with self._lock:
            if bid in self._benches:
                logger.warning("benchmark %s is already running here", bid)
                return
            self._benches[bid] = stop
        threading.Thread(
            target=self._run_bench, args=(dict(op), bid, stop),
            name=f"bench-{bid}", daemon=True,
        ).start()

    def _run_bench(self, op: Mapping[str, Any], bid: str, stop: threading.Event) -> None:
        """The benchmark itself; every ending says so with an `exit`.

        The library's `RemoteBench` is over at `bench_done`, `fatal` or
        `exit`; a body that ends with none of them reads as this processor
        leaving. So each ending here sends `exit` after whatever it had to
        say -- except when this processor IS leaving (`shutdown`), when it
        says nothing more at all, exactly as a session does.
        """
        import time

        from mokuro_bunko.ocr.utilization import first_gpu_device, sampler_for

        try:
            sink = self.client.open_events(bid)
        except OSError as e:
            logger.error("could not open the events body for benchmark %s: %s", bid, e)
            with self._lock:
                self._benches.pop(bid, None)
            return
        if sink.ended:
            self._react(bid, sink)
            with self._lock:
                self._benches.pop(bid, None)
            return
        workspace = self.storage / "bench" / Path(bid).name
        sampler = None
        session = None

        def say(head: Mapping[str, Any]) -> None:
            if not self._leaving.is_set():
                sink.send(head)

        try:
            spec = op.get("spec")
            if not isinstance(spec, dict):
                say({"event": "fatal", "error": "a bench op needs a spec"})
                return
            row = self._row(spec)
            sample = workspace / "pages"
            sample.mkdir(parents=True, exist_ok=True)
            try:
                fetched = self.fetcher.fetch(
                    str(op.get("sample") or ""), size=None, cancel=stop,
                    label=f"the benchmark sample {bid}",
                )
            except FetchCancelled:
                say({"event": "fatal", "error": "cancelled"})
                return
            except LibraryTransportError as e:
                self._lost_library(f"benchmark {bid}: {e}")
                return
            except (TransferFault, OSError) as e:
                say({"event": "fatal",
                     "error": f"could not fetch the benchmark sample: {e}"[:300]})
                return
            try:
                with zipfile.ZipFile(fetched.placement.read_path) as zf:
                    for info in zf.infolist():
                        if info.is_dir():
                            continue
                        if stop.is_set():
                            say({"event": "fatal", "error": "cancelled"})
                            return
                        (sample / Path(info.filename).name).write_bytes(zf.read(info))
            finally:
                fetched.release()
            processor = self._processor_for(row)
            session = processor.open_bench(
                row, sample, workspace / "bench.log",
                precision_only=bool(op.get("precision_only")),
            )
            override = os.environ.get("MOKURO_PROCESSOR_RUNNER")
            if override:
                session.command[1] = override
            # Addendum 9's busy% has to be sampled where the work happens,
            # so it is sampled HERE and stamped on each trial; the library
            # keeps whatever a trial already carries.
            if stop.is_set():
                # Cancelled while the runner was being prepared: it never
                # starts (the loop below would only kill it a poll later).
                say({"event": "fatal", "error": "cancelled"})
                return
            sampler = sampler_for(first_gpu_device(row.pools)).start()
            spawned = time.monotonic()
            if not session.start():
                refused = session.poll_event(timeout=5.0) or {}
                say({"event": "fatal", "error": str(refused.get("error") or "")})
                return
            while True:
                if stop.is_set():
                    say({"event": "fatal", "error": "cancelled"})
                    return
                polled = session.poll_event(timeout=1.0)
                if polled is None:
                    continue
                event: dict[str, Any] = dict(polled)
                kind = event.get("event")
                if kind == "exit":
                    return
                if kind == "bench_trial":
                    first = event.get("first_emission_at")
                    last = event.get("last_emission_at")
                    if first is not None and last is not None:
                        event.update(
                            {
                                key: value
                                for key, value in sampler.means(
                                    spawned + float(first), spawned + float(last)
                                ).items()
                                if event.get(key) is None
                            }
                        )
                say(event)
        except Exception as e:  # noqa: BLE001 - one benchmark's failure
            logger.exception("remote benchmark %s failed", bid)
            say({"event": "fatal", "error": f"{type(e).__name__}: {e}"[:300]})
        finally:
            with self._lock:
                self._benches.pop(bid, None)
            if sampler is not None:
                sampler.stop()
            if session is not None:
                session.kill()
                session.wait(timeout=5.0)
            shutil.rmtree(workspace, ignore_errors=True)
            say({"event": "exit", "returncode": None})
            sink.close()

    # -- the two things only this machine can do ---------------------------

    def _feed(self, bridged: _Bridged) -> None:
        """This session's volumes, one at a time, for as long as it lives.

        The feeder never dies: whatever one volume raises, that claim goes
        back to the library as ``local`` and the next is still fed -- a dead
        feeder would leave every later claim to 600 s of pings and then a
        blamed innocent.

        A volume still queued when the session ends is left ALONE: the
        library puts every claim it never heard a terminal event for back on
        the queue untouched.
        """
        while True:
            op = bridged.work.get()
            if op is None:
                return
            if bridged.stopping.is_set():
                logger.info(
                    "volume %r was never fed: session %s had already ended",
                    op.get("claim"), bridged.sid,
                )
                continue
            try:
                self._feed_one(bridged, op)
            except Exception as e:  # noqa: BLE001 - the feeder NEVER dies
                logger.exception("feeding %s failed", op.get("claim"))
                self._give_back(
                    bridged, str(op.get("claim") or ""), "local", f"{type(e).__name__}: {e}"
                )

    def _feed_one(self, bridged: _Bridged, op: Mapping[str, Any]) -> None:
        """Fetch one archive whole, verify it, and hand it to the runner.

        Placed, it is this function's until it is handed over (the ``finally``
        releases it on every other way out); handed over, it is the pump's,
        which releases it before forwarding the claim's terminal event.
        ``fetch {state: ready}`` goes out only once the runner's pipe took the
        op: that is what makes the claim DELIVERED on the library.
        """
        claim = str(op.get("claim") or "")
        archive_path = str(op.get("archive") or "")
        volume_title = str(op.get("volume_title") or "volume")
        # Every string off the wire that becomes part of a path on THIS
        # machine is reduced to a basename first. The library sends
        # basenames, so nothing here changes what a well-behaved one asked
        # for; a `..` or a separator in any of them would otherwise put a
        # file outside the workspace this volume owns. (`sidecar_name` is
        # also checked against the library's own copy when the file arrives
        # back, and `hint` becomes a `mkdtemp` prefix, which cannot hold a
        # separator at all.)
        hint = Path(volume_title).name or "volume"
        # The ARCHIVE's stem, not the volume title's: the thumbnail rule is
        # keyed on it, and the runner reads a file named /proc/<pid>/fd/<n>.
        stem = Path(PurePosixPath(archive_path).stem).name or hint
        sidecar_name = Path(str(op.get("sidecar_name") or f"{volume_title}.mokuro")).name
        key = f"fetch:{claim}"
        where = "/".join(PurePosixPath(archive_path).parts[-2:])
        fetched: FetchedArchive | None = None
        workspace: Path | None = None
        handed = False
        try:
            fetched = self.fetcher.fetch(
                archive_path,
                size=_as_int(op.get("size")),
                cancel=bridged.stopping,
                progress=lambda head: bridged.sink.offer(
                    key, {"event": "fetch", **head, "id": claim}
                ),
                label=f"{claim} {where}",
            )
            # Inside the try: a full disk here is a return, not a dead feeder.
            workspace = bridged.processor.new_workspace(hint)
            volume = SessionVolume(
                id=claim,
                archive=Path(fetched.runner_path),
                stem=stem,
                workspace=workspace,
                output=workspace / sidecar_name,
                cache_dir=workspace / "_ocr" / bridged.generation.id / stem,
                detect_dir=workspace / "_detect" / bridged.generation.id,
                log=self.storage / "logs" / f"{hint}.{claim}.log",
                title=str(op.get("title") or ""),
                volume=volume_title,
                title_uuid=op.get("title_uuid"),
                volume_uuid=op.get("volume_uuid"),
            )
            with bridged.lock:
                bridged.volumes[claim] = volume
                bridged.archives[claim] = fetched
                if fetched.verdict:
                    bridged.damaged_at_library[claim] = damaged_at_library_note(fetched)
            handed = True  # from here the pump, `_drop` and `_finish` own both
            if bridged.stopping.is_set():
                # The session ended while this one was verified: the library
                # settles its own claim.
                self._drop(bridged, claim)
                return
            if not bridged.session.submit(volume):
                # The pipe is gone, so the session is over: the claim goes
                # back with the others on `exit`.
                logger.warning("the runner would not take volume %s", claim)
                self._drop(bridged, claim)
                return
            bridged.sink.withdraw(key)
            self._report(
                bridged, {"event": "fetch", "id": claim, "state": "ready", **fetched.summary()}
            )
        except FetchCancelled:
            logger.info("volume %s: its download ended with its session", claim)
            return
        except LibraryTransportError as e:
            # Not this volume's, and not the next one's either: the account
            # was refused. Nothing is reported for it; the whole processor
            # steps away, so every claim goes back unrecorded, and registers
            # again (`_lost_library`) -- which a revoked account cannot.
            self._lost_library(f"volume {claim}: {e}")
            return
        except TransferFault as e:
            self._give_back(bridged, claim, e.kind, str(e), e.head())
            return
        finally:
            if not handed:
                if fetched is not None:
                    fetched.release()
                if workspace is not None:
                    shutil.rmtree(workspace, ignore_errors=True)

    def _give_back(
        self,
        bridged: _Bridged,
        claim: str,
        kind: str,
        error: str,
        counters: Mapping[str, Any] | None = None,
    ) -> None:
        """Tell the library this claim never reached the runner, and why.

        Never a failure: the library decides what a returned claim means from
        its own file (design section 6.2). Nothing is said once the session
        is stopping or the processor is leaving -- the library settles its
        own claims then.
        """
        if not claim:
            return
        bridged.sink.withdraw(f"fetch:{claim}")
        if bridged.stopping.is_set() or bridged.leaving.is_set() or self._leaving.is_set():
            logger.info("volume %s: not returned (%s): its session is ending", claim, kind)
            return
        logger.error("volume %s: giving it back (%s): %s", claim, kind, error)
        self._report(
            bridged,
            {"event": "volume_returned", "id": claim, "class": kind,
             "error": error[:300], **dict(counters or {})},
        )

    def _pump(self, bridged: _Bridged) -> None:
        """Every event the local runner produces, forwarded verbatim.

        Two things are added. Before a `volume_done` goes out, the sidecar the
        runner just wrote is read off this machine's disk and sent as the
        frame ahead of it, so the library has the file before it is told the
        volume is finished. And a claim's archive is released BEFORE its
        terminal event is forwarded -- the runner is done with it by then --
        so the top-up that event triggers never finds three held here; its
        workspace, the slow part, goes after.
        """
        try:
            while True:
                event = bridged.session.poll_event(timeout=1.0)
                if event is None:
                    continue
                kind = event.get("event")
                claim = str(event.get("id") or "")
                terminal = kind in ("volume_done", "volume_failed")
                if terminal:
                    self._release_archive(bridged, claim)
                if terminal and claim in bridged.cancelled:
                    # Cancelled: the library records nothing for it, so
                    # neither do we.
                    self._drop(bridged, claim)
                    continue
                if kind == "volume_done" and not self._send_sidecar(bridged, claim):
                    # A `volume_done` whose file never went is worse than a
                    # failure: the library would look for a sidecar at the
                    # path a local session would have left it, find nothing,
                    # and report a mystery against a runner that did its job.
                    kind = "volume_failed"
                    event = {
                        "event": kind, "id": claim,
                        "error": "the finished sidecar could not be sent from the processor",
                    }
                note = bridged.damaged_at_library.get(claim)
                if kind == "volume_failed" and note is not None:
                    event = {**event, "error": str(event.get("error") or "") + note}
                self._report(bridged, event)
                if terminal:
                    self._drop(bridged, claim)
                if kind == "exit":
                    return
        finally:
            self._finish(bridged)

    def _report(self, bridged: _Bridged, head: Mapping[str, Any], payload: bytes = b"") -> bool:
        """One event up the events body. False when it will not go.

        What a body that will not take it means is not decided here: the
        sink hands that to :meth:`_on_sink_end`, from whichever thread found
        the end first -- this one, or the ping thread.

        Nothing goes once this processor is LEAVING. `shutdown` kills the
        runner, and everything that follows -- its `exit` with a signal
        status, a `fatal` for a feed the kill cut short -- would reach the
        library as a runner that died on a volume, and the library blames
        the oldest volume in flight for that. A processor switched off is
        nobody's failure (spec sections 3 rule 4 and 6), and the library
        tells the two apart by the one thing left: a body that ends WITHOUT
        its runner's `exit` is a processor that went away, and every claim
        it held goes back unrecorded.
        """
        if bridged.leaving.is_set() or self._leaving.is_set():
            return False
        return bridged.sink.send(head, payload)

    def _lost_library(self, reason: str) -> None:
        """The library cannot be read from: step away, register again.

        Every session is marked LEAVING first, so not one more event reaches
        the library -- to it, this processor simply went away, which returns
        every claim unrecorded (spec section 3 rule 4) rather than failing
        each volume in turn for a fault that is not theirs. Ending the
        assignment stream sends `serve` back to register: a library that is
        merely restarting answers it; an account that was revoked or whose
        password changed is refused, and a refused login ends this processor
        non-zero (spec section 6).
        """
        logger.error("the library could not be read from (%s); registering again", reason)
        self._leaving.set()
        with self._lock:
            sessions = list(self._sessions.values())
            benches = list(self._benches.values())
        for bridged in sessions:
            bridged.leaving.set()
            bridged.stopping.set()
        for stop in benches:
            stop.set()
        self.fetcher.abort()
        self.client.close()

    def _on_sink_end(self, bridged: _Bridged, sink: EventSink) -> None:
        """The library ended this session's body: the session is over.

        A refusal costs this session whatever it says -- there is no way to
        report anything else through a body the library has closed.
        """
        self._react(bridged.sid, sink)
        bridged.stopping.set()
        self.fetcher.abort(bridged.stopping)
        bridged.session.kill()

    def _react(self, sid: str, sink: EventSink) -> None:
        """What a refusal's `code` asks of this processor as a whole.

        Three of the six codes mean the registration itself is void, which
        only a fresh one fixes. Ending the assignment stream is how that is
        asked for: `serve`'s loop registers again the moment it returns.
        """
        action = sink.action
        if action == ACTION_REREGISTER:
            logger.warning(
                "session %s: the library wants a fresh registration (%s)", sid, sink.code
            )
            self.client.close()
        elif action:
            logger.info("session %s: the events body ended (%s)", sid, action)
        else:
            logger.info("session %s: the library ended the events body", sid)

    def _send_sidecar(self, bridged: _Bridged, claim: str) -> bool:
        """The finished file, framed ahead of the event that announces it."""
        with bridged.lock:
            volume = bridged.volumes.get(claim)
        if volume is None:
            logger.error("a sidecar was due for %r, which this session never held", claim)
            return False
        try:
            blob = volume.output.read_bytes()
        except OSError as e:
            logger.error("could not read %s: %s", volume.output, e)
            return False
        return self._report(
            bridged, {"event": "sidecar", "id": claim, "name": volume.output.name}, blob
        )

    def _release_archive(self, bridged: _Bridged, claim: str) -> None:
        with bridged.lock:
            fetched = bridged.archives.pop(claim, None)
        if fetched is not None:
            fetched.release()

    def _drop(self, bridged: _Bridged, claim: str) -> None:
        self._release_archive(bridged, claim)
        with bridged.lock:
            volume = bridged.volumes.pop(claim, None)
            bridged.damaged_at_library.pop(claim, None)
        if volume is not None:
            shutil.rmtree(volume.workspace, ignore_errors=True)

    def _finish(self, bridged: _Bridged) -> None:
        with self._lock:
            self._sessions.pop(bridged.sid, None)
        bridged.stopping.set()
        self.fetcher.abort(bridged.stopping)
        bridged.work.put(None)
        with bridged.lock:
            leftovers = list(bridged.volumes.values())
            archives = list(bridged.archives.values())
            bridged.volumes.clear()
            bridged.archives.clear()
        for fetched in archives:
            fetched.release()
        for volume in leftovers:
            shutil.rmtree(volume.workspace, ignore_errors=True)
        bridged.sink.close()

    def shutdown(self) -> None:
        """End every session. Nothing in flight is recorded anywhere.

        Each session is marked LEAVING before its runner is touched, so not
        one event of the kill reaches the library (see :meth:`_report`): its
        body closes without an `exit`, which is how the library knows the
        processor left rather than a runner crashing on a volume.
        """
        self._leaving.set()
        with self._lock:
            sessions = list(self._sessions.values())
            benches = list(self._benches.values())
        # Every session and benchmark goes quiet BEFORE any runner is
        # touched, so no event of any kill reaches the library.
        for bridged in sessions:
            bridged.leaving.set()
            bridged.stopping.set()
        for stop in benches:
            stop.set()
        self.fetcher.abort()
        for bridged in sessions:
            bridged.session.kill()
            bridged.session.wait(timeout=5.0)
            self._finish(bridged)
