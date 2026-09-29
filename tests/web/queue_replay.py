"""Replay recorded /queue/api/status sequences into the real queue page, watched.

The queue page is plain files with no build step, so the honest way to see
what it does between jobs is to serve THEM -- not a copy -- from a server whose
status endpoint answers a recorded payload sequence one step per poll, and to
watch the page from before its first script runs:

* a MutationObserver logs every insertion and removal of a machine card
  (``.machine``, or ``.mline`` at minimal), a lane (``.lane``) or the
  container they live in, even inside a removed or added subtree -- an
  ``insertBefore`` that moves a card is a removal and an insertion too --
  and ANY element added to or removed from a card that is on the page, or
  hidden or shown in it;
* a ``layout-shift`` PerformanceObserver logs every shift and its sources,
  and whether every one of them is a pending row or comes after the pending
  list (the queue's own: a started volume leaves the list);
* a requestAnimationFrame sampler logs, whenever it changes, the box of every
  card and lane, of the machines block, the "Machines" heading, the Speed
  section, the "Pending OCR" heading and the top of the pending list, and
  the width the page lays out in (a page scrollbar that comes and goes);
  every frame in which anything inside a card -- the card itself, a lane, a
  line of text -- is drawn below full opacity; each card's SHAPE (the tag
  and classes of every element in it, in order, and which are not drawn);
  and what its fields say (each lane's volume slot and numbers, and the
  on-deck field).

Every entry carries the step on screen when it happened: the step whose
response the page had received last (``X-Replay-Step``). `analyze` then keeps
what happened while the set of connected machines was the one before it
("steady" steps): a machine connecting or leaving, or the display level
changing, may re-lay the page out; nothing else may. A card's shape and its
insides are held to more: they may not change at ANY step, for as long as the
card is on the page, and its fields are checked against every payload.

`load_sequence` / `probe_sequence` build the steps; `replay` runs one in a browser;
`analyze` turns the log into `Findings`.
"""

from __future__ import annotations

import copy
import json
import math
import threading
import time
from bisect import bisect_right
from dataclasses import dataclass, field
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from playwright.sync_api import Browser

WEB_ROOT = Path(__file__).resolve().parents[2] / "src" / "mokuro_bunko"
FIXTURES = Path(__file__).resolve().parent / "fixtures"

STATIC = {
    "/queue/": ("queue/web/index.html", "text/html"),
    "/queue/index.html": ("queue/web/index.html", "text/html"),
    "/queue/queue.js": ("queue/web/queue.js", "application/javascript"),
    "/queue/styles.css": ("queue/web/styles.css", "text/css"),
    "/_static/shared.css": ("static/shared.css", "text/css"),
    "/_static/nav.js": ("static/nav.js", "application/javascript"),
}

# The page's own poll delay is 1000 ms; a replay shortens it so a sequence of
# a hundred-odd steps takes seconds. Every step still stays on screen for at
# least this long, and a step that changed the set of machines for SETTLE_S.
POLL_MS = 100
SETTLE_S = 0.8


def load_sequence(level: str) -> dict[str, Any]:
    """A recorded sequence (tests/web/fixtures/queue-replay-<level>.json)."""
    path = FIXTURES / f"queue-replay-{level}.json"
    return json.loads(path.read_text(encoding="utf-8"))


# --- instants: served as if the server had just built the payload ------------

ISO_KEYS = ("eta_at", "queue_done_at")
EPOCH_KEYS = ("until", "since", "disconnected_at")


def rebase(obj: Any, shift: float) -> Any:
    """Every instant in a payload moved by ``shift`` seconds."""
    if isinstance(obj, list):
        return [rebase(x, shift) for x in obj]
    if not isinstance(obj, dict):
        return obj
    out: dict[str, Any] = {}
    for key, value in obj.items():
        if key in ISO_KEYS and isinstance(value, str) and value:
            try:
                when = datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() + shift
            except ValueError:
                pass
            else:
                value = datetime.fromtimestamp(round(when), tz=timezone.utc).strftime(
                    "%Y-%m-%dT%H:%M:%SZ"
                )
        elif key in EPOCH_KEYS and isinstance(value, (int, float)) and not isinstance(value, bool):
            value = value + shift
        else:
            value = rebase(value, shift)
        out[key] = value
    return out


def _names(payload: dict[str, Any]) -> list[Any]:
    return sorted(m.get("name") for m in payload.get("machines") or [])


def settle_steps(steps: list[dict[str, Any]]) -> set[int]:
    """The steps that may re-lay the page out: the first, and every one where
    the SET of connected machines or the display level changed."""
    out = {0}
    for i in range(1, len(steps)):
        before, after = steps[i - 1]["payload"], steps[i]["payload"]
        if _names(before) != _names(after) or before.get("level") != after.get("level"):
            out.add(i)
    return out


# --- synthetic what-ifs, built on a recorded payload --------------------------


def both_running(sequence: dict[str, Any]) -> dict[str, Any]:
    """The first recorded payload with both machines reading."""
    return both_running_step(sequence)[0]


def both_running_step(sequence: dict[str, Any]) -> tuple[dict[str, Any], float]:
    for step in sequence["steps"]:
        machines = step["payload"].get("machines") or []
        if len(machines) == 2 and all(
            m.get("jobs")
            and m["jobs"][0].get("state") == "running"
            and (m["jobs"][0].get("percent") or 0) > 0
            for m in machines
        ):
            return step["payload"], step["t"]
    raise AssertionError("no step with both machines reading")


def probe_sequence(sequence: dict[str, Any]) -> dict[str, Any]:
    """What the recorded runs never happened to send, one change at a time.

    A recorded payload with both machines reading, then each variant for a
    step or two and back: a row that cannot start here, a busy host, a held
    machine, an idle one, one loading and one waiting, nothing on deck and
    something on deck, very long names, the on-deck volume inside ``jobs``
    (more volumes than lanes), the machines listed in the other order, and --
    at ``detailed`` -- a longer pipeline with a verdict, none at all, and the
    speed list going 1 -> 0 -> 1 -> 2 -> 1 lines. Every one keeps the same set
    of machines, so none may move or resize anything, or change a card's
    shape.
    """
    base, t = both_running_step(sequence)
    level = base.get("level")

    def variant(change: Any) -> dict[str, Any]:
        payload = copy.deepcopy(base)
        change(payload)
        return payload

    def cannot_start(p: dict[str, Any]) -> None:
        p["machines"][0]["cannot_start"] = [
            {
                "generation": "paddle-manga-animetext",
                "until": 1790270000.0,
                "error": "no GPU execution provider",
            },
        ]

    def host_busy(p: dict[str, Any]) -> None:
        p["machines"][0]["jobs"][0]["host_busy"] = True

    def held(p: dict[str, Any]) -> None:
        m = p["machines"][0]
        m["jobs"], m["state"], m["next"] = [], "held", []
        m["held"] = {"reason": "downloads failing", "until": 1790270000.0, "error": "404"}

    def idle(p: dict[str, Any]) -> None:
        m = p["machines"][0]
        m["jobs"], m["state"], m["next"] = [], "idle", []

    def standby(p: dict[str, Any]) -> None:
        m = p["machines"][0]
        m["jobs"], m["state"], m["next"] = [], STANDBY, []

    def loading(p: dict[str, Any]) -> None:
        m = p["machines"][0]
        m["state"] = "loading"
        m["jobs"][0].update(
            state="loading",
            status="starting",
            percent=0,
            done_pages=0,
            startup_seconds=12,
            eta_at=None,
            eta_seconds=None,
            pipeline=None,
        )

    def waiting(p: dict[str, Any]) -> None:
        m = p["machines"][0]
        m["state"] = "waiting"
        m["jobs"][0].update(
            state="waiting",
            status="starting",
            percent=0,
            done_pages=0,
            startup_seconds=None,
            pipeline=None,
        )

    def nothing_on_deck(p: dict[str, Any]) -> None:
        for m in p["machines"]:
            m["next"] = []

    def two_on_deck(p: dict[str, Any]) -> None:
        m = p["machines"][0]
        m["next"] = [
            {"series": "Series Q", "volume": "Volume 07", "generation": "gen-q"},
            {"series": "Series Q", "volume": "Volume 08", "generation": "gen-q"},
        ]

    def long_names(p: dict[str, Any]) -> None:
        m = p["machines"][0]
        m["jobs"][0]["series"] = "Series With A Very Long Name That Will Not Fit In One Line " * 2
        m["jobs"][0]["volume"] = "Volume 01, a very long volume title that keeps on going " * 2
        m["jobs"][0]["generation"] = "a-layer-name-long-enough-to-be-cut-anywhere-it-is-shown"
        m["next"] = [dict(m["jobs"][0])]
        m["label"] = "a machine label that is quite long (Some GPU 9999 XTX Ultra)"
        m["cannot_start"] = [
            {
                "generation": "a-layer-name-long-enough-to-be-cut-anywhere-it-is-shown",
                "until": 1790270000.0,
                "error": "a very long error message " * 6,
            },
        ] * 3

    def on_deck_in_jobs(p: dict[str, Any]) -> None:
        m = p["machines"][0]
        extra = copy.deepcopy(m["jobs"][0])
        extra.update(
            volume="Volume 99", percent=0, done_pages=0, state="waiting", status="starting"
        )
        m["jobs"].append(extra)

    def swapped_order(p: dict[str, Any]) -> None:
        p["machines"] = list(reversed(p["machines"]))

    def long_pipeline(p: dict[str, Any]) -> None:
        p["machines"][0]["jobs"][0]["pipeline"] = {
            "verdict": "engine starved 41% waiting on detect \u2014 widen detect, and a verdict "
            "long enough to need more than one line at a phone's width",
            "bottleneck": "detect",
            "stages": [
                {
                    "key": key,
                    "name": key,
                    "device": device,
                    "workers": 2,
                    "fused": False,
                    "busy_pct": busy,
                    "blocked_pct": 1.0,
                    "starved_pct": 100.0 - busy - 1.0,
                    "queue": {
                        "name": key + "->next",
                        "capacity": 4,
                        "mean_depth": 1.5,
                        "max_depth": 3,
                    },
                }
                for key, device, busy in (
                    ("detect", "cpu", 97.0),
                    ("engine", "gpu:0", 57.0),
                    ("post", "cpu", 19.0),
                )
            ],
        }

    def no_pipeline(p: dict[str, Any]) -> None:
        p["machines"][0]["jobs"][0]["pipeline"] = None

    def speed(n: int) -> Any:
        def change(p: dict[str, Any]) -> None:
            p["speed"] = [
                {"generation": f"gen-{i}", "pages_per_minute": 100.0 + i, "machines": 1}
                for i in range(n)
            ]

        return change

    changes: list[Any] = [
        cannot_start,
        host_busy,
        held,
        idle,
        standby,
        loading,
        waiting,
        nothing_on_deck,
        two_on_deck,
        long_names,
        on_deck_in_jobs,
        swapped_order,
    ]
    if level == "detailed":
        changes += [long_pipeline, no_pipeline]
    frames = [base, base]
    for change in changes:
        frames += [variant(change), variant(change), base, base]
    if level == "detailed":
        frames += [variant(speed(n)) for n in (1, 1, 0, 1, 2, 1, 0, 0, 2)] + [base]
    return {
        "level": level,
        "t0_wall": sequence.get("t0_wall"),
        "steps": [{"t": t, "payload": payload} for payload in frames],
    }


# The state a connected machine with nothing running is in while the
# earliest-finish scheduler leaves the queued work it could run to faster
# machines (`queue.shape.STATE_STANDBY`), and what its empty lane says.
STANDBY = "standby"
STANDBY_TEXT = "Faster machines will finish the queue sooner"


def standby_sequence(sequence: dict[str, Any]) -> dict[str, Any]:
    """A recorded payload with both machines reading, then the first machine
    idle -> standby -> running -> idle -> standby -> running, and the second
    machine into standby and back while the first reads. The set of machines
    never changes, so nothing may move, resize or change shape."""
    base, t = both_running_step(sequence)

    def machine(state: str, which: int = 0) -> dict[str, Any]:
        payload = copy.deepcopy(base)
        m = payload["machines"][which]
        if state != "running":
            m["jobs"], m["state"], m["next"] = [], state, []
        return payload

    frames = [base, base]
    for state in ("idle", STANDBY, "running", "idle", STANDBY, "running"):
        frames += [machine(state), machine(state)]
    frames += [machine(STANDBY, 1), machine(STANDBY, 1), base, base]
    return {
        "level": base.get("level"),
        "t0_wall": sequence.get("t0_wall"),
        "steps": [{"t": t, "payload": payload} for payload in frames],
    }


# --- the server -----------------------------------------------------------------


class ReplayServer:
    """Serves the page, and the sequence one step per poll.

    Each poll after the first gets the next step, except that a settle step
    (a machine connected or left) stays for `settle_s` first: the page may
    re-lay itself out for it, and what it does next is measured from a page
    at rest. After the last step every poll is a 304.
    """

    def __init__(self, sequence: dict[str, Any], settle_s: float = SETTLE_S) -> None:
        self.steps = sequence["steps"]
        self.level = sequence.get("level") or self.steps[0]["payload"].get("level") or "normal"
        self.t0_wall = sequence.get("t0_wall")
        self.settle = settle_steps(self.steps)
        self.settle_s = settle_s
        self.current = -1
        self.served_at = 0.0
        self.lock = threading.Lock()
        self.httpd: ThreadingHTTPServer | None = None

    @property
    def url(self) -> str:
        assert self.httpd is not None
        return f"http://127.0.0.1:{self.httpd.server_address[1]}"

    def finished(self, linger: float = 0.6) -> bool:
        with self.lock:
            return (
                self.current == len(self.steps) - 1 and time.monotonic() - self.served_at >= linger
            )

    def _next(self, sent_etag: str | None) -> tuple[int, bool]:
        """(step, fresh): the step to answer with, and whether to send it."""
        with self.lock:
            hold = self.settle_s if self.current in self.settle else 0.0
            if self.current < len(self.steps) - 1 and (
                self.current < 0 or time.monotonic() - self.served_at >= hold
            ):
                self.current += 1
                self.served_at = time.monotonic()
                return self.current, True
            return self.current, sent_etag != f'"s{self.current}"'

    def _body(self, index: int) -> bytes:
        step = self.steps[index]
        payload = step["payload"]
        if self.t0_wall:
            payload = rebase(payload, time.time() - (self.t0_wall + step["t"]))
        return json.dumps(payload).encode()

    def _handler(self) -> type[BaseHTTPRequestHandler]:
        server = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args: Any) -> None:  # quiet
                pass

            def _send(
                self, code: int, body: bytes, ctype: str, extra: dict[str, str] | None = None
            ) -> None:
                self.send_response(code)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Cache-Control", "no-store")
                for key, value in (extra or {}).items():
                    self.send_header(key, value)
                self.end_headers()
                if body:
                    self.wfile.write(body)

            def _json(self, payload: Any) -> None:
                self._send(200, json.dumps(payload).encode(), "application/json")

            def do_GET(self) -> None:  # noqa: N802
                path = self.path.split("?")[0]
                if path in STATIC:
                    rel, ctype = STATIC[path]
                    self._send(200, (WEB_ROOT / rel).read_bytes(), ctype + "; charset=utf-8")
                elif path == "/api/nav/config":
                    self._json(
                        {
                            "home_enabled": True,
                            "catalog_enabled": False,
                            "queue_show_in_nav": True,
                            "queue_public_access": True,
                            "registration_enabled": False,
                        }
                    )
                elif path == "/queue/api/config":
                    self._json(
                        {"show_in_nav": True, "public_access": True, "display": server.level}
                    )
                elif path == "/queue/api/status":
                    index, fresh = server._next(self.headers.get("If-None-Match"))
                    headers = {"ETag": f'"s{index}"', "X-Replay-Step": str(index)}
                    if fresh:
                        self._send(200, server._body(index), "application/json", headers)
                    else:
                        self._send(304, b"", "application/json", headers)
                else:
                    self._send(404, b"not found", "text/plain")

        return Handler

    def __enter__(self) -> ReplayServer:
        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), self._handler())
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        return self

    def __exit__(self, *exc: Any) -> None:
        assert self.httpd is not None
        self.httpd.shutdown()
        self.httpd.server_close()


# --- the page's instrumentation ---------------------------------------------------

POLL_SHIM_JS = """
(() => {
  const realSetTimeout = window.setTimeout;
  window.setTimeout = function (fn, delay, ...rest) {
    if (delay === 1000) delay = %d;
    return realSetTimeout.call(this, fn, delay, ...rest);
  };
})();
"""

INSTRUMENT_JS = r"""
(() => {
  const L = window.__reflow = {steps: [], mut: [], inner: [], shifts: [], geo: [], fades: [],
    shape: [], texts: [], vw: [], errors: []};
  window.__step = -1;
  const ids = new WeakMap();
  let nextId = 1;
  const idOf = n => { if (!ids.has(n)) ids.set(n, nextId++); return ids.get(n); };
  const desc = n => {
    if (!n || n.nodeType !== 1) return n ? n.nodeName : null;
    let s = n.tagName.toLowerCase();
    if (n.id) s += '#' + n.id;
    const cls = typeof n.className === 'string' ? n.className.trim() : '';
    if (cls) s += '.' + cls.split(/\s+/).join('.');
    if (n.dataset && n.dataset.key != null) s += '[' + n.dataset.key.replace(/[\u0000-\u001f]/g, '|') + ']';
    return s;
  };
  const TRACK = '.machine, .mline, .lane, .machines__list, .mlines';
  const CARD = '.machine, .mline';

  // Which step is on screen: the last status response the page received.
  const realFetch = window.fetch;
  window.fetch = function (input) {
    const url = String((input && input.url) || input);
    return realFetch.apply(this, arguments).then(r => {
      const s = r.headers.get('X-Replay-Step');
      if (url.indexOf('/queue/api/status') >= 0 && r.status === 200 && s != null) {
        window.__step = +s;
        L.steps.push({step: +s, t: performance.now()});
      }
      return r;
    });
  };

  // Cards, lines, lanes and their containers coming and going; and, inside
  // a card that is on the page, ANY element added or removed (a card's
  // elements are made once, when its machine connects: text may change,
  // elements may not). The `hidden` attribute set or cleared on anything
  // inside a card is logged too.
  new MutationObserver(records => {
    const step = window.__step;
    for (const r of records) {
      if (r.type === 'attributes') {
        const card = r.target.closest && r.target.closest(CARD);
        if (card && r.target !== card) {
          L.inner.push({step, card: desc(card), kind: 'hidden ' + r.target.hasAttribute('hidden'),
                        node: desc(r.target)});
        }
        continue;
      }
      const inside = r.target.nodeType === 1 && r.target.closest(CARD);
      for (const [kind, nodes] of [['removed', r.removedNodes], ['inserted', r.addedNodes]]) {
        for (const n of nodes) {
          if (n.nodeType !== 1) continue;
          if (inside) L.inner.push({step, card: desc(inside), kind, node: desc(n)});
          const hits = n.matches(TRACK) ? [n] : [];
          hits.push(...n.querySelectorAll(TRACK));
          for (const h of hits) L.mut.push({step, kind, node: desc(h)});
        }
      }
    }
  }).observe(document, {childList: true, subtree: true, attributes: true,
                        attributeFilter: ['hidden']});

  // A shift is the queue's own when everything that moved is a pending row
  // or comes after the pending list: a volume starting leaves the list, and
  // what is under the list closes up.
  const inQueue = n => {
    const list = document.getElementById('pending-ocr-list');
    try {
      return !!list && (list.contains(n) ||
        !!(list.compareDocumentPosition(n) & Node.DOCUMENT_POSITION_FOLLOWING));
    } catch (e) { return false; }
  };
  try {
    new PerformanceObserver(list => {
      for (const e of list.getEntries()) {
        const sources = e.sources || [];
        L.shifts.push({t: e.startTime, value: e.value, sources: sources.map(s => desc(s.node)),
          queue: sources.length > 0 && sources.every(s => inQueue(s.node))});
      }
    }).observe({type: 'layout-shift', buffered: true});
  } catch (e) { L.errors.push('layout-shift: ' + e); }

  const last = new Map();
  const lastShape = new Map();
  const lastTexts = new Map();
  let lastWidth = null;
  function box(n, topOnly) {
    if (!n.isConnected || n.offsetParent === null) return 'hidden';
    const r = n.getBoundingClientRect();
    const v = [r.x + window.scrollX, r.y + window.scrollY, r.width, r.height]
      .map(x => Math.round(x * 10) / 10);
    return (topOnly ? v.slice(0, 2) : v).join(',');
  }
  function watched() {
    const out = [];
    for (const n of document.querySelectorAll('.machine, .mline')) out.push([n, 'card', false]);
    for (const n of document.querySelectorAll('.machine .lane, .mline .lane')) out.push([n, 'lane', false]);
    const add = (n, what, topOnly) => { if (n) out.push([n, what, topOnly]); };
    add(document.getElementById('machines'), 'machines', false);
    add(document.querySelector('#machines-section > h2'), 'machines heading', false);
    add(document.getElementById('machines-none'), 'machines-none', false);
    add(document.getElementById('processing-hold'), 'processing-hold', false);
    add(document.getElementById('speed-section'), 'speed section', false);
    const count = document.getElementById('pending-ocr-count');
    add(count && count.parentElement, 'pending heading', false);
    add(document.getElementById('pending-ocr-list'), 'pending list top', true);
    return out;
  }
  function sample() {
    try {
      const t = performance.now(), step = window.__step;
      // The width the page lays out in: the window's, less the page
      // scrollbar's room when it takes (or keeps) some.
      const width = document.body ? Math.round(document.body.getBoundingClientRect().width * 10) / 10 : null;
      if (width !== lastWidth) { L.vw.push({t, step, width}); lastWidth = width; }
      for (const [n, what, topOnly] of watched()) {
        const id = idOf(n);
        const k = box(n, topOnly);
        // The pending list is hidden while the queue is empty; where it
        // starts is what may not move.
        if (topOnly && k === 'hidden') continue;
        if (last.get(id) !== k) {
          L.geo.push({t, step, id, what, node: desc(n), from: last.has(id) ? last.get(id) : null, to: k});
          last.set(id, k);
        }
      }
      // Everything inside a card, at the opacity it is really drawn with;
      // the card's SHAPE -- the tag and classes of every element in it, in
      // order, and which of them are not drawn at all -- whenever it
      // changes; and what its fields say (the lanes' volume slots and the
      // on-deck field) whenever that changes.
      for (const card of document.querySelectorAll(CARD)) {
        const id = idOf(card);
        const drawn = new Map();
        const cs = getComputedStyle(card);
        let min = +cs.opacity, worst = card;
        drawn.set(card, min);
        const shape = [];
        for (const n of card.querySelectorAll('*')) {
          const s = getComputedStyle(n);
          const o = +s.opacity * (drawn.has(n.parentElement) ? drawn.get(n.parentElement) : 1);
          drawn.set(n, o);
          if (o < min) { min = o; worst = n; }
          const cls = typeof n.className === 'string' ? n.className.trim().split(/\s+/).join('.') : '';
          shape.push(n.tagName.toLowerCase() + (cls ? '.' + cls : '') +
            (n.hidden ? '[hidden]' : '') + (s.display === 'none' ? '{none}' : '') +
            (s.visibility !== 'visible' ? '{' + s.visibility + '}' : ''));
        }
        if (min < 0.999) {
          L.fades.push({t, step, card: desc(card), opacity: Math.round(min * 1000) / 1000, node: desc(worst)});
        }
        const sig = shape.join(' ');
        if (lastShape.get(id) !== sig) {
          L.shape.push({t, step, id, card: desc(card), sig});
          lastShape.set(id, sig);
        }
        const lanes = [...card.querySelectorAll('.lane')].map(lane => {
          const what = lane.querySelector('.job__volume, .mline__what');
          const series = lane.querySelector('.job__series');
          return {what: what ? what.textContent : null, series: series ? series.textContent : null,
                  pill: (lane.querySelector('.state-pill') || {}).textContent || null,
                  pages: (lane.querySelector('.job__pages') || {}).textContent || null,
                  pct: (lane.querySelector('.job__pct') || {}).textContent || null,
                  eta: (lane.querySelector('.job__eta') || {}).textContent || null};
        });
        const next = card.querySelector('.machine__next-text, .mline__next-text');
        const texts = JSON.stringify({machine: card.dataset.key, lanes,
                                      next: next ? next.textContent : null});
        if (lastTexts.get(id) !== texts) {
          L.texts.push({t, step, id, texts: JSON.parse(texts)});
          lastTexts.set(id, texts);
        }
      }
    } catch (e) { L.errors.push('sample: ' + e); }
    requestAnimationFrame(sample);
  }
  requestAnimationFrame(sample);
})();
"""


# --- one replay -------------------------------------------------------------------


@dataclass
class Findings:
    """What moved while the machines stayed the same (steady steps only)."""

    level: str
    steps: int
    settle: list[int]
    dom: list[dict[str, Any]] = field(default_factory=list)
    shifts: list[dict[str, Any]] = field(default_factory=list)
    queue_shifts: list[dict[str, Any]] = field(default_factory=list)
    moves: list[dict[str, Any]] = field(default_factory=list)
    fades: list[dict[str, Any]] = field(default_factory=list)
    gutter: list[dict[str, Any]] = field(default_factory=list)
    # At ANY step, for as long as a card is on the page: an element added to
    # or removed from it (or its `hidden` flipped), a change of its shape,
    # and a field that did not say what the payload said.
    inner: list[dict[str, Any]] = field(default_factory=list)
    reshaped: list[dict[str, Any]] = field(default_factory=list)
    fields: list[str] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)
    served: int = 0
    cards: int = 0
    idle_lanes: int = 0
    standby_lanes: int = 0
    empty_on_deck: int = 0

    @property
    def shift_score(self) -> float:
        return round(sum(s["value"] for s in self.shifts), 5)

    @property
    def max_box_delta(self) -> float:
        """The largest move or resize of a card or lane, in CSS px (inf: one
        appeared or disappeared)."""
        worst = 0.0
        for m in self.moves:
            if m["what"] not in ("card", "lane"):
                continue
            worst = max(worst, _delta(m["from"], m["to"]))
        return worst

    def problems(self) -> list[str]:
        out: list[str] = []
        if self.dom:
            out.append(
                f"{len(self.dom)} card/line/lane insertions or removals: "
                + _first(self.dom, lambda m: f"step {m['step']} {m['kind']} {m['node']}")
            )
        if self.shifts:
            out.append(
                f"{len(self.shifts)} layout shifts (score {self.shift_score}): "
                + _first(
                    self.shifts,
                    lambda s: f"step {s['step']} {s['value']:.4f} {', '.join(s['sources'])}",
                )
            )
        if self.moves:
            out.append(
                f"{len(self.moves)} boxes moved or resized: "
                + _first(
                    self.moves,
                    lambda m: f"step {m['step']} {m['what']} {m['node']} {m['from']} -> {m['to']}",
                )
            )
        if self.fades:
            steps = sorted({f["step"] for f in self.fades})
            out.append(
                f"{len(self.fades)} frames drew a card's content below full opacity "
                f"(steps {steps[:12]}): "
                + _first(
                    self.fades,
                    lambda f: f"step {f['step']} {f['card']} -> {f['node']} at {f['opacity']}",
                )
            )
        if self.gutter:
            out.append(
                "the width the page lays out in changed (a scrollbar came or went): "
                + _first(self.gutter, lambda v: f"step {v['step']} width {v['width']}")
            )
        if self.inner:
            out.append(
                f"{len(self.inner)} elements added, removed, hidden or shown inside a "
                "card: "
                + _first(
                    self.inner, lambda m: f"step {m['step']} {m['card']}: {m['kind']} {m['node']}"
                )
            )
        if self.reshaped:
            out.append(
                f"{len(self.reshaped)} cards changed shape: "
                + _first(self.reshaped, lambda r: f"{r['card']} at step {r['step']}: {r['diff']}")
            )
        if self.fields:
            out.append(
                f"{len(self.fields)} fields said the wrong thing: " + _first(self.fields, str)
            )
        if self.errors:
            out.append("page errors: " + "; ".join(self.errors[:5]))
        return out

    def measurements(self) -> dict[str, Any]:
        return {
            "insertions_removals": len(self.dom),
            "layout_shifts": len(self.shifts),
            "shift_score": self.shift_score,
            "queue_shifts": len(self.queue_shifts),
            "box_changes": len(self.moves),
            "max_box_delta_px": self.max_box_delta,
            "faded_frames": len(self.fades),
            "scrollbar_flips": len(self.gutter),
            "inner_changes": len(self.inner),
            "shape_changes": len(self.reshaped),
            "field_errors": len(self.fields),
            "cards": self.cards,
            "idle_lanes_checked": self.idle_lanes,
            "standby_lanes_checked": self.standby_lanes,
            "empty_on_deck_checked": self.empty_on_deck,
        }


def _first(items: list[Any], fmt: Any, n: int = 4) -> str:
    text = "; ".join(fmt(x) for x in items[:n])
    return text + (f"; ... (+{len(items) - n})" if len(items) > n else "")


def _delta(a: str | None, b: str | None) -> float:
    if not a or not b or a == "hidden" or b == "hidden":
        return math.inf
    va, vb = [float(x) for x in a.split(",")], [float(x) for x in b.split(",")]
    return round(max(abs(x - y) for x, y in zip(va, vb, strict=True)), 1)


def analyze(log: dict[str, Any], steps: list[dict[str, Any]], level: str) -> Findings:
    settle = settle_steps(steps)

    def steady(step: Any) -> bool:
        return isinstance(step, int) and step >= 0 and step not in settle

    arrivals = sorted((s["t"], s["step"]) for s in log["steps"])
    times = [t for t, _ in arrivals]

    def step_at(t: float) -> int:
        i = bisect_right(times, t) - 1
        return arrivals[i][1] if i >= 0 else -1

    findings = Findings(
        level=level,
        steps=len(steps),
        settle=sorted(settle),
        served=len({s["step"] for s in log["steps"]}),
        errors=list(log.get("errors") or []) + list(log.get("page_errors") or []),
    )
    findings.dom = [m for m in log["mut"] if steady(m["step"])]
    for shift in log["shifts"]:
        step = step_at(shift["t"])
        if steady(step):
            (findings.queue_shifts if shift["queue"] else findings.shifts).append(
                dict(shift, step=step)
            )
    findings.moves = [g for g in log["geo"] if steady(g["step"])]
    findings.fades = [f for f in log["fades"] if steady(f["step"])]
    findings.gutter = [v for v in log["vw"][1:] if steady(v["step"])]
    findings.inner = list(log.get("inner") or [])
    shapes: dict[int, list[dict[str, Any]]] = {}
    for entry in log.get("shape") or []:
        shapes.setdefault(entry["id"], []).append(entry)
    findings.cards = len(shapes)
    for history in shapes.values():
        for before, after in zip(history, history[1:], strict=False):
            findings.reshaped.append(
                {
                    "card": after["card"],
                    "step": after["step"],
                    "diff": _shape_diff(before["sig"], after["sig"]),
                }
            )
    _check_fields(findings, log.get("texts") or [], steps, level)
    return findings


def _shape_diff(a: str, b: str) -> str:
    ta, tb = a.split(" "), b.split(" ")
    for i, (x, y) in enumerate(zip(ta, tb, strict=False)):
        if x != y:
            return f"element {i}: {x} -> {y}"
    return f"{len(ta)} -> {len(tb)} elements"


# What an empty cell of a lane holds: a non-breaking space keeps its line.
BLANK = "\u00a0"


def _what(item: dict[str, Any]) -> str:
    return (item.get("series") + " \u00b7 " if item.get("series") else "") + (
        item.get("volume") or ""
    )


def _check_fields(
    findings: Findings, texts: list[dict[str, Any]], steps: list[dict[str, Any]], level: str
) -> None:
    """Each card's fields against the payload on screen: an idle lane says
    "Idle" (its other cells blank) -- or, for a machine in standby, why it is
    waiting -- a busy one names its volume, and the on-deck field names the
    next volume -- or says "None".

    What a card said at step N is the last thing it was seen saying while N
    was on screen (the page renders a payload within a frame or two of
    receiving it, and a step stays up for far longer).
    """
    said: dict[tuple[int, int], dict[str, Any]] = {}
    for entry in texts:
        said[(entry["step"], entry["id"])] = entry["texts"]
    ids = sorted({entry["id"] for entry in texts})
    latest: dict[int, dict[str, Any]] = {}
    for index, step in enumerate(steps):
        for card in ids:
            if (index, card) in said:
                latest[card] = said[(index, card)]
        machines = {m.get("name"): m for m in step["payload"].get("machines") or []}
        for card in ids:
            shown = latest.get(card)
            machine = machines.get(shown["machine"]) if shown else None
            if machine is None:
                continue
            where = f"step {index} {shown['machine']}"
            lanes = shown["lanes"]
            slots = max(1, machine.get("slots") or 1)
            if len(lanes) != slots:
                findings.fields.append(f"{where}: {len(lanes)} lanes for {slots} slots")
                continue
            jobs = machine.get("jobs") or []
            if not jobs and machine.get("state") != "held":
                standing_by = machine.get("state") == STANDBY
                for lane in lanes:
                    if standing_by:
                        findings.standby_lanes += 1
                    else:
                        findings.idle_lanes += 1
                    wanted = {
                        "what": STANDBY_TEXT if standing_by else "Idle",
                        "pct": BLANK,
                        "eta": BLANK,
                    }
                    if level != "minimal":
                        wanted.update(series=BLANK, pages=BLANK)
                    else:
                        wanted.update(pill="Standby" if standing_by else "Idle")
                    wrong = {k: lane.get(k) for k, v in wanted.items() if lane.get(k) != v}
                    if wrong:
                        kind = "a standby" if standing_by else "an idle"
                        findings.fields.append(f"{where}: {kind} lane shows {wrong}")
            if len(jobs) > slots:
                # More volumes than lanes: which ones hold a lane depends on
                # which held one before; the rest are on deck.
                if shown["next"] in (None, "None"):
                    findings.fields.append(
                        f"{where}: {len(jobs)} volumes for {slots} lanes, and nothing on deck"
                    )
                continue
            for job in jobs:
                want = _what(job) if level == "minimal" else (job.get("volume") or BLANK)
                if want not in [lane["what"] for lane in lanes]:
                    findings.fields.append(
                        f"{where}: {want!r} is in no lane ({[lane['what'] for lane in lanes]})"
                    )
            upcoming = list(machine.get("next") or [])
            if not upcoming:
                findings.empty_on_deck += 1
                if shown["next"] != "None":
                    findings.fields.append(
                        f"{where}: nothing on deck, but it says {shown['next']!r}"
                    )
            elif not (shown["next"] or "").startswith(_what(upcoming[0])):
                findings.fields.append(
                    f"{where}: on deck is {_what(upcoming[0])!r}, it says {shown['next']!r}"
                )


def replay(
    browser: Browser,
    sequence: dict[str, Any],
    *,
    width: int,
    height: int,
    reduced_motion: bool,
    poll_ms: int = POLL_MS,
    timeout_s: float | None = None,
) -> Findings:
    """Play `sequence` into the page in `browser`; what moved while it played."""
    steps = sequence["steps"]
    level = sequence.get("level") or steps[0]["payload"].get("level") or "normal"
    with ReplayServer(sequence) as server:
        context = browser.new_context(
            viewport={"width": width, "height": height},
            reduced_motion="reduce" if reduced_motion else "no-preference",
        )
        try:
            context.add_init_script(POLL_SHIM_JS % poll_ms)
            context.add_init_script(INSTRUMENT_JS)
            page = context.new_page()
            page_errors: list[str] = []
            page.on("pageerror", lambda e: page_errors.append(str(e)))
            page.goto(server.url + "/queue/")
            budget = timeout_s or 20 + len(steps) * (poll_ms / 1000 + 0.3) + len(server.settle)
            deadline = time.monotonic() + budget
            while not server.finished():
                if time.monotonic() > deadline:
                    raise AssertionError(f"replay stalled at step {server.current} of {len(steps)}")
                page.wait_for_timeout(100)
            log = page.evaluate("window.__reflow")
            log["page_errors"] = page_errors
        finally:
            context.close()
    findings = analyze(log, steps, level)
    if findings.served != len(steps):
        findings.errors.append(f"the page rendered {findings.served} of {len(steps)} steps")
    return findings
