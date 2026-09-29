"""A scriptable stand-in for ``engine_runner.py --serve`` / ``--bench``.

The real runner needs a GPU, several gigabytes of model weights and the
engines environment. None of that is available in a test, and none of it is
what the SERVER side has to get right: what the server has to get right is
the protocol -- the ops it writes, the events it reads, and what it does when
a runner is slow, fails one volume, dies mid-volume, wedges, prints rubbish
on stdout or exits without being asked to.

So this script speaks exactly that protocol and nothing else, and is told how
to behave by a JSON script in ``$FAKE_RUNNER_SCRIPT`` (a file path, or the
JSON itself). Every key is optional:

```json
{
  "startup_seconds": 0.0,     // reported in `ready` ("first page after X s")
  "ready_delay": 0.0,         // seconds before `ready` is emitted
  "no_ready": false,          // never emit `ready` at all
  "pages": 3,                 // pages per volume, unless "volumes" says otherwise
  "page_delay": 0.0,          // seconds between page events
  "volume_delay": 0.0,        // seconds before a volume's first page
  "stats": true,              // emit a `stats` event while a volume runs
  "garbage": [],              // non-protocol lines to print at startup
  "spawn_fatal": null,        // emit `fatal` with this text and exit 1
  "wedge_after": null,        // stop emitting anything after N volume ops
  "die_after_pages": null,    // exit(9) mid-volume after this many pages
  "exit_unclosed_after": null,// exit(0) after N volumes WITHOUT being closed
  "read_archive": false,      // really open each archive (see below)
  "volumes": {                // per-volume overrides, keyed by the op id
    "v2": {"fail": "every page failed"},
    "v3": {"pages": 10, "no_sidecar": true}
  },
  "bench": {...}              // see `_run_bench`
}
```

It writes a real (tiny but valid) mokuro sidecar at each volume's ``output``
before saying ``volume_done``, so the server's collection path -- validate,
normalize, move into the library -- runs for real.

With ``read_archive`` on, an ARCHIVE volume is really read, the way the real
runner reads one: the archive is opened with ``zipfile`` (a processor's
``/proc/<pid>/fd/<n>`` included), its pages are the ones
``engine_runner.archive_pages(archive, stem)`` names -- the thumbnail rule
keyed on the op's ``stem`` -- a page that will not read is blanked in its own
place, and an archive that will not open (or has no pages, or no readable
page) fails the volume with the real runner's words. Its session log gets
``archive <path> stem=<stem> sha256=<hex> pages=<n> blank=<k>``. It is off by
default, so other tests' made-up archive paths are left alone.

A volume names its source in the ``volume`` op: an ``archive`` (a remote
processor's is its verified copy, ``/proc/<pid>/fd/<n>``, with the library
archive's ``stem`` beside it) or an extracted ``input`` directory.
"""

from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path
from typing import Any


def _emit(event: dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(event) + "\n")
    sys.stdout.flush()


def _load_script() -> dict[str, Any]:
    raw = os.environ.get("FAKE_RUNNER_SCRIPT", "")
    if not raw:
        return {}
    text = raw
    candidate = Path(raw)
    try:
        if candidate.is_file():
            text = candidate.read_text(encoding="utf-8")
    except OSError:
        pass
    try:
        data = json.loads(text)
    except ValueError:
        return {}
    return data if isinstance(data, dict) else {}


def _log(script: dict[str, Any], argv: list[str], message: str) -> None:
    """Prose goes to the session log, never to stdout."""
    path = None
    if "--session-log" in argv:
        path = argv[argv.index("--session-log") + 1]
    if not path:
        return
    try:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(message + "\n")
    except OSError:
        pass
    del script


def _read_archive(argv: list[str], op: dict[str, Any]) -> tuple[list[str], str | None]:
    """The archive's page names, or why the volume fails -- as the runner would."""
    import hashlib
    import zipfile

    from mokuro_bunko.ocr.engine_runner import archive_members, reading_order

    archive = str(op.get("archive") or "")
    stem = op.get("stem")
    try:
        digest = hashlib.sha256(Path(archive).read_bytes()).hexdigest()
        members = archive_members(Path(archive), stem)
        zf = zipfile.ZipFile(archive)
    except Exception as e:  # noqa: BLE001 - the volume's failure, by name
        _log({}, argv, f"archive {archive} stem={stem} unreadable: {e}")
        return [], f"{type(e).__name__}: {e}"
    pages = reading_order(list(members))
    blank = 0
    with zf:
        for rel in pages:
            try:
                zf.read(members[rel])
            except Exception as e:  # noqa: BLE001 - one page, in its place
                blank += 1
                _log({}, argv, f"[runner] ERROR page {rel}: cannot be read from the archive: {e}")
    _log({}, argv, f"archive {archive} stem={stem} sha256={digest} pages={len(pages)} "
                   f"blank={blank}")
    if not pages:
        where = f"{stem}.cbz" if stem else archive
        return [], f"no page images found in {where}"
    if blank == len(pages):
        return [], "every page failed"
    return [rel.as_posix() for rel in pages], None


def _write_sidecar(op: dict[str, Any], pages: int, names: list[str] | None = None) -> None:
    output = Path(op["output"])
    output.parent.mkdir(parents=True, exist_ok=True)
    names = names or [f"page_{n:03d}.jpg" for n in range(pages)]
    payload = {
        "version": "0.0.0-fake",
        "title": op.get("title") or "",
        "title_uuid": op.get("title_uuid") or "",
        "volume": op.get("volume") or "",
        "volume_uuid": op.get("volume_uuid") or "fake-volume-uuid",
        "pages": [{"img_path": name, "blocks": []} for name in names],
        "chars": 0,
        "ocr_engine": {"id": "fake", "generation": "fake"},
    }
    tmp = output.with_name(output.name + ".tmp")
    tmp.write_text(json.dumps(payload), encoding="utf-8")
    os.replace(tmp, output)


def _stats_block(pages: int, elapsed: float) -> dict[str, Any]:
    """The raw counter shape ``pipeline.json`` has always had."""
    return {
        "elapsed_seconds": elapsed,
        "items": pages,
        "bottleneck": "detect",
        "stages": [
            {
                "key": "detect",
                "name": "detect",
                "device": "cpu",
                "workers": 1,
                "items": pages,
                "busy_seconds": elapsed * 0.97,
                "blocked_seconds": elapsed * 0.01,
                "starved_seconds": 0.0,
            },
            {
                "key": "engine",
                "name": "engine read",
                "device": "gpu",
                "workers": 1,
                "device_bound": True,
                "items": pages,
                "busy_seconds": elapsed * 0.4,
                "blocked_seconds": 0.0,
                "starved_seconds": elapsed * 0.5,
            },
        ],
        "queues": [
            {"name": "detect->engine", "capacity": 1, "mean_depth": 0.1, "max_depth": 1},
        ],
    }


def _run_serve(script: dict[str, Any], argv: list[str]) -> int:
    if script.get("spawn_fatal"):
        _emit({"event": "fatal", "error": str(script["spawn_fatal"])})
        return 1
    for line in script.get("garbage") or []:
        sys.stdout.write(str(line) + "\n")
        sys.stdout.flush()
    if script.get("ready_delay"):
        time.sleep(float(script["ready_delay"]))
    if not script.get("no_ready"):
        _emit(
            {
                "event": "ready",
                "startup_seconds": float(script.get("startup_seconds", 0.0)),
                "weights": {},
                "stage_workers": {"detect": 1, "engine": 1},
                "queue_capacity": {"detect": 1},
                "pipeline": "detect -> engine",
            }
        )

    per_volume = script.get("volumes") or {}
    default_pages = int(script.get("pages", 3))
    wedge_after = script.get("wedge_after")
    exit_unclosed_after = script.get("exit_unclosed_after")
    seen = 0
    done = 0

    for raw_line in sys.stdin:
        line = raw_line.strip()
        if not line:
            continue
        try:
            op = json.loads(line)
        except ValueError:
            continue
        if not isinstance(op, dict):
            continue
        if op.get("op") == "close":
            break
        if op.get("op") != "volume":
            continue
        seen += 1
        _log(script, argv, f"volume {op.get('id')} <- {op.get('archive') or op.get('input')}")
        if wedge_after is not None and seen > int(wedge_after):
            # Accepted and then silent: exactly the shape the server's wedge
            # timeout exists for.
            while True:
                time.sleep(3600)
        rules = per_volume.get(str(op.get("id")), {})
        if _run_volume(
            script, rules, op, int(rules.get("pages", default_pages)),
            read=bool(script.get("read_archive")) and bool(op.get("archive"))
            and "pages" not in rules,
            argv=argv,
        ):
            done += 1
            if exit_unclosed_after is not None and done >= int(exit_unclosed_after):
                os._exit(0)
    return 0


def _run_volume(
    script: dict[str, Any],
    rules: dict[str, Any],
    op: dict[str, Any],
    pages: int,
    *,
    read: bool = False,
    argv: list[str] | None = None,
) -> bool:
    """One volume's events, by the script's rules.

    ``read`` reads the archive for real first (``read_archive``). Returns
    whether the volume COMPLETED, which is what ``exit_unclosed_after``
    counts.
    """
    if script.get("volume_delay"):
        time.sleep(float(script["volume_delay"]))
    names: list[str] | None = None
    if read:
        names, failure = _read_archive(argv or [], op)
        if failure is not None:
            _emit({"event": "volume_started", "id": op.get("id"), "pages": len(names)})
            _emit({"event": "volume_failed", "id": op.get("id"), "error": failure})
            return False
        pages = len(names)
    _emit({"event": "volume_started", "id": op.get("id"), "pages": pages})
    start = time.monotonic()
    for page in range(1, pages + 1):
        if script.get("page_delay"):
            time.sleep(float(script["page_delay"]))
        if rules.get("die_after_pages") is not None and page > int(rules["die_after_pages"]):
            os._exit(9)
        _emit({"event": "page", "id": op.get("id"), "done": page, "total": pages})
    elapsed = max(time.monotonic() - start, 0.01)
    if script.get("stats", True):
        _emit({"event": "stats", "pipeline": _stats_block(pages, elapsed)})
    if rules.get("fail"):
        _emit({"event": "volume_failed", "id": op.get("id"), "error": str(rules["fail"])})
        return False
    if not rules.get("no_sidecar"):
        _write_sidecar(op, pages, names)
    _emit(
        {
            "event": "volume_done",
            "id": op.get("id"),
            "pages": pages,
            "failed_pages": 0,
            "seconds": elapsed,
            "stats": _stats_block(pages, elapsed),
        }
    )
    return True


def _run_bench(script: dict[str, Any], argv: list[str]) -> int:
    """``--bench``: one ready event, some trials, one done event."""
    bench = script.get("bench") or {}
    if bench.get("fatal"):
        _emit({"event": "fatal", "error": str(bench["fatal"])})
        return 1
    pages = int(bench.get("pages", 32))
    if "--input" in argv:
        sample = Path(argv[argv.index("--input") + 1])
        try:
            found = sum(1 for p in sample.iterdir() if p.is_file())
            pages = found or pages
        except OSError:
            pass
    # ADDENDUM 9: a trial carries the WINDOW its rate was read over -- the
    # span of the page emissions, in seconds since this process started --
    # and the server turns that into the utilization means beside it.
    trials = bench.get("trials") or [
        {
            "n": 1,
            "note": "auto",
            "stage_workers": {"detect": 1, "engine": 1},
            "queue_capacity": {"detect": 1, "engine": 1},
            "seconds": 21.3,
            "pages_per_second": 1.5,
            "window_seconds": 20.0,
            "pages_measured": 31,
            "passes": 2,
            "short_window": False,
            "first_emission_at": 12.0,
            "last_emission_at": 32.0,
            "accepted": True,
            "verdict": "engine starved 38% waiting on detect — widen detect",
            "bottleneck": "detect",
            "stages": [{"key": "detect", "workers": 1, "busy_pct": 97, "starved_pct": 0,
                        "blocked_pct": 1}],
            "queues": [{"name": "detect->engine", "capacity": 1, "mean_depth": 0.1,
                        "max_depth": 1}],
        },
        {
            "n": 2,
            "note": "detect=3",
            "stage_workers": {"detect": 3, "engine": 1},
            "queue_capacity": {"detect": 1, "engine": 1},
            "seconds": 15.2,
            "pages_per_second": 2.1,
            "window_seconds": 21.0,
            "pages_measured": 45,
            "passes": 2,
            "short_window": False,
            "first_emission_at": 34.0,
            "last_emission_at": 55.0,
            "accepted": True,
            "verdict": None,
            "bottleneck": "engine",
            "stages": [{"key": "detect", "workers": 3, "busy_pct": 60, "starved_pct": 5,
                        "blocked_pct": 20}],
            "queues": [{"name": "detect->engine", "capacity": 3, "mean_depth": 1.4,
                        "max_depth": 3}],
        },
    ]
    _emit(
        {
            "event": "bench_ready",
            "startup_seconds": float(bench.get("startup_seconds", 11.2)),
            "model_load_seconds": float(bench.get("model_load_seconds", 9.8)),
            "min_window_seconds": float(bench.get("min_window_seconds", 20.0)),
            "pages": pages,
            "tunable": True,
            "max_trials": int(bench.get("max_trials", 8)),
            "stage_keys": list(bench.get("stage_keys") or ["detect", "engine"]),
            "stage_device": dict(
                bench.get("stage_device") or {"detect": "cpu", "engine": "gpu:0"}
            ),
        }
    )
    if bench.get("hang"):
        while True:
            time.sleep(3600)
    for trial in trials:
        if bench.get("trial_delay"):
            time.sleep(float(bench["trial_delay"]))
        _emit(
            {
                "event": "bench_progress",
                "trial": trial.get("n"),
                "pass_index": 1,
                "pages_done": pages // 2,
                "pages": pages,
                "stage_workers": trial.get("stage_workers") or {},
                "pages_per_second": trial.get("pages_per_second"),
                "window_seconds": trial.get("window_seconds"),
                "pages_measured": trial.get("pages_measured"),
            }
        )
        _emit({"event": "bench_trial", **trial})
    best = bench.get("best") or {
        "trial": 2,
        "stage_workers": {"detect": 3},
        "queue_capacity": {},
        "pages_per_second": 2.1,
        "seconds_per_page": 0.476,
        "speedup": 1.4,
        "window_seconds": 21.0,
        "pages_measured": 45,
        "passes": 2,
        "short_window": False,
    }
    _emit(
        {
            "event": "bench_done",
            "baseline": bench.get("baseline")
            or {"pages_per_second": 1.5, "seconds_per_page": 0.667},
            "best": best,
            # What the recognizer ran at: a record, never a decision. fp32
            # unless the script says otherwise, as the real runner reports
            # for a torch recognizer -- but none when the script's ``best``
            # carries one: that is a runner older than the policy, which put
            # its measured winner there instead (None: send none at all).
            **(
                {"precision": ran}
                if (
                    ran := bench.get(
                        "precision", None if "precision" in best else "fp32"
                    )
                )
                else {}
            ),
            "peak_rss_mb": bench.get("peak_rss_mb", 2410),
            "peak_vram_mb": bench.get("peak_vram_mb", 3120),
        }
    )
    return 0


def main(argv: list[str]) -> int:
    script = _load_script()
    if "--bench" in argv:
        return _run_bench(script, argv)
    return _run_serve(script, argv)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
