(function () {
  "use strict";

  // Polling. The server answers an unchanged poll with a bodiless 304 (it
  // keeps a queue-state version and an ETag per display level), so a poll a
  // second costs it next to nothing. A hidden tab backs off; a failing server
  // is retried at the old 3 s interval, then slower.
  var FAST_INTERVAL = 1000;
  var HIDDEN_INTERVAL = 10000;
  var FALLBACK_INTERVAL = 3000;
  var MAX_BACKOFF = 30000;
  // How far past the last server update the bars are allowed to run on their
  // own. A stalled engine must not creep to 99% on extrapolation alone.
  var MAX_EXTRAPOLATION_MS = 30000;
  var TICK_MS = 250;

  var $ = function (id) { return document.getElementById(id); };
  var $backendLine = $("backend-line");
  var $backend = $("backend");
  var $machines = $("machines");
  var $machinesNone = $("machines-none");
  var $speedSection = $("speed-section");
  var $speedList = $("speed-list");
  var $pendingOcrCount = $("pending-ocr-count");
  var $pendingOcrList = $("pending-ocr-list");
  var $pendingOcrEmpty = $("pending-ocr-empty");
  var $pendingOcrOrder = $("pending-ocr-order");
  var $pendingMore = $("pending-more");
  var $thumbSection = $("thumb-section");
  var $pendingThumbCount = $("pending-thumb-count");
  var $pendingThumbText = $("pending-thumb-text");
  var $failedSection = $("failed-section");
  var $failedCount = $("failed-count");
  var $failedList = $("failed-list");
  var $runOrder = $("run-order");
  var $runOrderList = $("run-order-list");
  var $processingHold = $("processing-hold");
  var $processingHoldText = $("processing-hold-text");
  var $heldRows = $("held-rows");
  var $skippedSection = $("skipped-section");
  var $skippedCount = $("skipped-count");
  var $skippedList = $("skipped-list");
  var $queueDone = $("queue-done");

  var queueConfig = { show_in_nav: false, public_access: true };
  var etag = null;
  var failures = 0;
  var pollTimer = null;
  var level = null;
  var reducedMotion = window.matchMedia
    ? window.matchMedia("(prefers-reduced-motion: reduce)")
    : { matches: false };

  function motion() {
    return !reducedMotion.matches;
  }

  function getSessionAuth() {
    try {
      return sessionStorage.getItem("mokuro_auth");
    } catch (e) {
      return null;
    }
  }

  function forgetStoredLogin() {
    try {
      sessionStorage.removeItem("mokuro_auth");
      sessionStorage.removeItem("mokuro_user");
    } catch (e) {
      /* storage unavailable: nothing stored to forget */
    }
    etag = null;
  }

  function logout() {
    sessionStorage.removeItem("mokuro_auth");
    sessionStorage.removeItem("mokuro_user");
    window.location.href = "/";
  }

  async function updateNav() {
    if (window.renderMokuroHeaderNav) {
      await window.renderMokuroHeaderNav("queue");
    }
  }

  // Sent whenever the viewer is logged in, public queue or not: an admin is
  // shown the raw errors and hardware names, and only the server can decide
  // who that is.
  function getStatusHeaders() {
    var headers = {};
    var auth = getSessionAuth();
    if (auth) headers.Authorization = "Basic " + auth;
    return headers;
  }

  // --- formatting -------------------------------------------------------

  function formatDuration(seconds) {
    if (seconds == null || !isFinite(seconds)) return "";
    seconds = Math.max(0, Math.round(seconds));
    if (seconds < 60) return seconds + "s";
    var m = Math.floor(seconds / 60);
    var s = seconds % 60;
    if (m < 60) return m + "m " + s + "s";
    var h = Math.floor(m / 60);
    m = m % 60;
    return h + "h " + m + "m";
  }

  // The server sends instants as ISO-8601 UTC and never formats a local
  // time -- it has no idea which zone anyone is reading in. This is where
  // that instant becomes a clock: the browser's own, with its own locale,
  // its own 12/24-hour habit and its own DST. The date is added only when
  // the instant is NOT today: a bare "02:15" on a queue that finishes
  // tomorrow morning is the one way this readout could actively mislead.
  function localClock(iso) {
    if (!iso) return "";
    var when = new Date(iso);
    if (isNaN(when.getTime())) return "";
    var time = when.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    var now = new Date();
    if (
      when.getFullYear() === now.getFullYear() &&
      when.getMonth() === now.getMonth() &&
      when.getDate() === now.getDate()
    ) {
      return time;
    }
    return when.toLocaleDateString([], { month: "short", day: "numeric" }) + " " + time;
  }

  function clockTime(epochSeconds) {
    if (typeof epochSeconds !== "number" || !isFinite(epochSeconds)) return "?";
    return new Date(epochSeconds * 1000).toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
    });
  }

  function ppm(value) {
    if (typeof value !== "number" || !isFinite(value) || value <= 0) return "";
    return value >= 10 ? String(Math.round(value)) : value.toFixed(1).replace(/\.0$/, "");
  }

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text != null) node.textContent = text;
    return node;
  }

  // Text that only changes the DOM when it really changed: an ETA ticking
  // every quarter second must not re-lay out (or flicker) the card.
  function setText(node, text) {
    if (node && node.textContent !== text) node.textContent = text;
  }

  function setHidden(node, hidden) {
    if (node && node.hidden !== hidden) node.hidden = hidden;
  }

  // An attribute set to `value`, or removed for null; untouched when equal.
  function setAttr(node, name, value) {
    if (value == null) {
      if (node.hasAttribute(name)) node.removeAttribute(name);
    } else if (node.getAttribute(name) !== value) {
      node.setAttribute(name, value);
    }
  }

  // --- keyed lists --------------------------------------------------------
  //
  // Every list on the page is reconciled by key instead of being rebuilt, so
  // a node that stays keeps its progress bar and the bar's width transition
  // carries it to the new value. Nothing here animates anything: a node that
  // arrives is simply there, one that goes is removed at once, nothing fades
  // and nothing slides.

  function reconcile(container, items, keyOf, create, update) {
    var existing = {};
    var child;
    for (child = container.firstElementChild; child; child = child.nextElementSibling) {
      existing[child.dataset.key] = child;
    }
    var keep = {};
    var cursor = container.firstElementChild;
    items.forEach(function (item, index) {
      var key = keyOf(item, index);
      if (keep[key]) key += "\u0003" + index;
      var node = existing[key];
      if (!node) {
        node = create(item, index);
        node.dataset.key = key;
      }
      keep[key] = true;
      update(node, item, index);
      if (node === cursor) {
        cursor = cursor.nextElementSibling;
      } else {
        container.insertBefore(node, cursor);
      }
    });
    Object.keys(existing).forEach(function (key) {
      if (!keep[key]) existing[key].remove();
    });
  }

  // A lane that switched to another volume (or to idle) keeps every element
  // it has, and nothing in it fades, blinks or moves: its text changes in
  // place and the state pill says what happened. (It used to blank its whole
  // content and fade it back in from nothing: the card "flashing out of
  // existence" at every job change.) Only the bar is special-cased -- it
  // JUMPS to the new volume's value instead of running backwards from 100%
  // to 0%: no transition while the new width is set, and the width committed
  // (the style flushed) before the transition comes back.
  function barJumps(lane, set) {
    var fill = lane.querySelector(".progress-bar__fill");
    if (!fill) {
      set();
      return;
    }
    fill.style.transition = "none";
    set();
    fill.getBoundingClientRect();
    requestAnimationFrame(function () { fill.style.transition = ""; });
  }

  // --- the running volumes, and the bars that keep moving ----------------
  //
  // Between two server updates a bar would sit still, then jump. It does not
  // have to: the lane knows where the volume was (percent, pages) when the
  // payload arrived and when it will be done (`eta_at`), so it moves at that
  // rate until the next update corrects it. Never to 100% (only the server
  // says a volume is done) and never for more than MAX_EXTRAPOLATION_MS past
  // the last update, so a stalled engine shows as stalled. With reduced
  // motion there is no extrapolation at all.

  var live = [];

  function track(node, job) {
    node._job = job;
    node._t0 = Date.now();
    node._etaMs = job && job.eta_at ? new Date(job.eta_at).getTime() : NaN;
    if (live.indexOf(node) < 0) live.push(node);
  }

  function stateOf(job) {
    if (!job) return "idle";
    if (job.state) return job.state;
    if (job.status === "starting") {
      return typeof job.startup_seconds === "number" && job.startup_seconds > 0
        ? "loading"
        : "waiting";
    }
    return "running";
  }

  function projected(node) {
    var job = node._job;
    if (!job) return { pct: 0, pages: null };
    var pct = job.percent || 0;
    var pages = typeof job.done_pages === "number" ? job.done_pages : null;
    var total = typeof job.total_pages === "number" ? job.total_pages : null;
    var running = stateOf(job) === "running" && (!job.status || job.status === "running");
    if (!motion() || !running || !isFinite(node._etaMs) || node._etaMs <= node._t0) {
      return { pct: pct, pages: pages };
    }
    var elapsed = Math.min(Date.now() - node._t0, MAX_EXTRAPOLATION_MS);
    var share = Math.max(0, Math.min(1, elapsed / (node._etaMs - node._t0)));
    if (pages != null && total) {
      var p = Math.max(pages, Math.min(total - 1, pages + (total - pages) * share));
      var frac = total > pages ? (p - pages) / (total - pages) : 0;
      return { pct: Math.min(99, pct + (100 - pct) * frac), pages: p };
    }
    return { pct: Math.min(99, pct + (100 - pct) * share), pages: pages };
  }

  function etaText(node) {
    var job = node._job;
    if (!job) return "";
    var at = localClock(job.eta_at);
    var state = stateOf(job);
    if (state === "loading") {
      // Nothing has come out of the engine yet, so there is no page rate:
      // what the model load costs is the only honest number.
      if (typeof job.startup_seconds === "number" && job.startup_seconds > 0) {
        var left = job.startup_seconds - (Date.now() - node._t0) / 1000;
        return "starting up (≈ " + formatDuration(Math.max(1, left)) + ")";
      }
      return "starting up";
    }
    if (state === "waiting") return at ? "done " + at : "waiting to start";
    if (job.status === "finalizing") return at ? "finishing — done " + at : "finishing";
    // The countdown is from the instant, on this browser's clock: a cached
    // payload's `eta_seconds` was right when it was built, not now.
    var seconds = isFinite(node._etaMs)
      ? (node._etaMs - Date.now()) / 1000
      : typeof job.eta_seconds === "number"
        ? job.eta_seconds - (Date.now() - node._t0) / 1000
        : null;
    var leftText = seconds != null && seconds > 0 ? formatDuration(seconds) : "";
    // minimal: a glance -- the clock, not the countdown.
    if (level === "minimal" && at) return "done " + at;
    if (at && leftText) return "done " + at + " (" + leftText + " left)";
    if (at) return "done " + at;
    if (leftText) return "ETA " + leftText;
    return "";
  }

  // An empty slot keeps its line: a non-breaking space holds the height. A
  // lane with nothing to read shows its cells blank -- a dash in every one
  // read as data that had gone missing -- and says what it is doing in the
  // title's place ("Idle", "Held ...", "Auto configuring ...").
  var HOLD = "\u00a0";
  // What a Speed row with nothing to report shows.
  var NONE = "\u2014";

  // A lane's numbers: the bar, pages, percent and time. Every one of them is
  // always written -- blank when there is nothing to say -- into a slot of
  // its own that never moves.
  function paint(node) {
    var job = node._job;
    var state = stateOf(job);
    var now = projected(node);
    var fill = node.querySelector(".progress-bar__fill");
    var showBar = state === "running";
    if (fill) fill.style.width = (showBar ? Math.max(0, Math.min(100, now.pct)) : 0).toFixed(2) + "%";
    setText(node.querySelector(".job__pct"), showBar ? Math.floor(now.pct) + "%" : HOLD);
    var pagesNode = node.querySelector(".job__pages");
    if (pagesNode) {
      var total = job && job.total_pages;
      setText(
        pagesNode,
        showBar && now.pages != null
          ? Math.floor(now.pages) + (total ? " / " + total : "") + " pages"
          : HOLD
      );
    }
    setText(node.querySelector(".job__eta"), etaText(node) || HOLD);
  }

  function tick() {
    if (document.hidden) return;
    live = live.filter(function (node) { return node.isConnected; });
    live.forEach(paint);
  }

  function jobKey(job) {
    return [job.series || "", job.volume || "", job.generation || ""].join("\u0001");
  }

  function genBadge(name) {
    return name ? el("span", "badge badge--gen", name) : null;
  }

  var STATE_LABEL = {
    running: "Running", loading: "Loading", waiting: "Waiting", idle: "Idle", held: "Held",
    configuring: "Configuring", standby: "Standby",
  };

  // The state lives in an ATTRIBUTE, never in a class: a card's element and
  // class structure is the same whatever its machine is doing.
  function setPill(pill, state) {
    setAttr(pill, "data-state", state);
    setText(pill, STATE_LABEL[state] || state);
  }

  // --- the Machines section -------------------------------------------------
  //
  // One card per connected machine (at `minimal`, one compact card), made ONCE
  // when the machine connects and removed only when it leaves. Its shape
  // never changes while it is here: every field it has -- the name and state
  // pill, what each lane is processing (or "Idle"), the bar, pages, percent,
  // time, and what is on deck (or "None") -- is always there, in the same
  // element, in the same place. Between jobs only the DATA in them changes:
  // text, an attribute (the state), the bar's width. Nothing inside a card is
  // ever added, removed, shown, hidden, faded or moved.

  // The one container of the cards. Made once per display level -- `render`
  // drops it when the level changes -- and after that only ever updated.
  var $list = null;

  function machinesList() {
    if (!$list) {
      $machines.textContent = "";
      $list = level === "minimal" ? el("ul", "mlines") : el("div", "machines__list");
      $machines.appendChild($list);
    }
    return $list;
  }

  // The order the machines are shown in: the order they connected. Those
  // already connected when the page opens keep the server's order; one that
  // connects later goes at the END, so no card already on the page moves for
  // it; one that leaves takes only its own card. Never re-sorted by what the
  // machines are doing, and whatever order a payload happens to list them in.
  var shownOrder = [];

  function inPlace(machines) {
    var byName = Object.create(null);
    machines.forEach(function (machine) { byName[machine.name] = machine; });
    var order = shownOrder.filter(function (name) { return name in byName; });
    machines.forEach(function (machine) {
      if (order.indexOf(machine.name) < 0) order.push(machine.name);
    });
    shownOrder = order;
    return order.map(function (name) { return byName[name]; });
  }

  // How many lanes a machine has: one per slot, busy or not, for as long as
  // it is connected -- never how many volumes it happens to hold this second.
  function laneCount(machine) {
    return Math.max(1, machine.slots || 1);
  }

  // The job each lane of `container` shows now (null: none).
  function shownJobs(container) {
    var shown = [];
    for (var lane = container.firstElementChild; lane; lane = lane.nextElementSibling) {
      shown.push(lane.getAttribute("data-job"));
    }
    return shown;
  }

  // Which lane each of a machine's volumes goes in. `shown` is the job key
  // each lane shows now: a volume still running stays in its lane, a new one
  // takes the first free lane. More volumes than lanes (the one on deck,
  // sent as running) never make a lane: they are `extra`, and go in the
  // on-deck field with the rest of what is on deck.
  function placeJobs(machine, shown) {
    var count = laneCount(machine);
    var lanes = [];
    for (var i = 0; i < count; i++) lanes.push(null);
    var rest = [];
    (machine.jobs || []).forEach(function (job) {
      var at = shown.indexOf(jobKey(job));
      if (at >= 0 && at < count && !lanes[at]) lanes[at] = job;
      else rest.push(job);
    });
    var extra = [];
    rest.forEach(function (job) {
      var free = lanes.indexOf(null);
      if (free >= 0) lanes[free] = job;
      else extra.push(job);
    });
    return {
      lanes: lanes.map(function (job, index) {
        return { machine: machine, job: job, index: index, many: count > 1 };
      }),
      upcoming: extra.concat(machine.next || []),
    };
  }

  // What a lane is doing: its volume's state, or -- with no volume -- idle,
  // held by the library, being benchmarked (configuring), or on standby
  // (the scheduler is leaving the queue to faster machines).
  function laneState(machine, job) {
    if (!job && (machine.state === "held" || machine.state === "configuring" ||
                 machine.state === "standby")) {
      return machine.state;
    }
    return stateOf(job);
  }

  // What a lane with nothing in it says, in the slot its volume's title takes.
  // A machine being benchmarked says which row: "Auto configuring hayai-nova"
  // for the automatic benchmark a new row or machine gets before it runs
  // there -- "Idle" meanwhile read as a machine the library had lost.
  function emptyLaneText(state, minimal, machine) {
    // Connected and able to run what is queued, but the queue finishes
    // sooner on the faster machines: not lost, not broken, just not needed.
    if (state === "standby") return "Faster machines will finish the queue sooner";
    if (state === "held") return minimal ? "Held — downloads failing" : "Held — its downloads keep failing";
    if (state === "configuring") {
      var line = (machine && machine.configuring) || {};
      return (line.auto ? "Auto configuring " : "Benchmarking ") +
        (line.generation || "an unsaved generation");
    }
    return "Idle";
  }

  function whatText(item) {
    return (item.series ? item.series + " · " : "") + (item.volume || "");
  }

  // Put a lane's job (or its absence) into it: attributes and text only.
  function fillLane(node, job, state) {
    var key = job ? jobKey(job) : null;
    var changed = node._filled && node.getAttribute("data-job") !== key;
    node._filled = true;
    setAttr(node, "data-job", key);
    setAttr(node, "data-state", state);
    var fill = function () {
      track(node, job);
      paint(node);
    };
    if (changed) barJumps(node, fill);
    else fill();
  }

  // The on-deck field: the next volume (series · volume, and its layer on a
  // card), "+ N more" when more wait behind it on this machine -- or "None".
  function fillOnDeck(node, upcoming, withLayer) {
    var first = upcoming[0];
    setText(
      node,
      first
        ? whatText(first) +
          (withLayer && first.generation ? " · " + first.generation : "") +
          (upcoming.length > 1 ? " + " + (upcoming.length - 1) + " more" : "")
        : "None"
    );
    node.title = upcoming.map(function (item) {
      return whatText(item) + (item.generation ? " · " + item.generation : "");
    }).join("\n");
    setAttr(node, "data-none", first ? null : "");
  }

  // --- minimal: one compact card per machine ---------------------------------
  //
  //   RIG-A  (Running)  Series A · Volume 02            33%  done 14:05
  //   [=====---------------------------------------------------------]
  //   On deck: Series B · Volume 02

  function createMini() {
    var node = el("li", "mline");
    node.appendChild(el("div", "mline__lanes"));
    var next = el("div", "mline__next");
    next.appendChild(el("span", "mline__next-label", "On deck:"));
    next.appendChild(el("span", "mline__next-text"));
    node.appendChild(next);
    return node;
  }

  function createMiniLane() {
    var node = el("div", "lane mline__lane");
    var head = el("div", "mline__head");
    head.appendChild(el("span", "mline__name"));
    head.appendChild(el("span", "state-pill"));
    head.appendChild(el("span", "mline__what"));
    var right = el("span", "mline__right");
    right.appendChild(el("span", "job__pct"));
    right.appendChild(el("span", "job__eta"));
    head.appendChild(right);
    node.appendChild(head);
    var bar = el("div", "progress-bar progress-bar--thin");
    bar.appendChild(el("div", "progress-bar__fill"));
    node.appendChild(bar);
    return node;
  }

  function updateMiniLane(node, lane) {
    var job = lane.job;
    var state = laneState(lane.machine, job);
    setText(node.querySelector(".mline__name"), lane.machine.name);
    var what = node.querySelector(".mline__what");
    var empty = job ? "" : emptyLaneText(state, true, lane.machine);
    setText(what, job ? whatText(job) : empty);
    // A narrow card cuts the line; the whole of it is on hover.
    setAttr(what, "title", empty || null);
    node.title = job && job.generation ? "generation: " + job.generation : "";
    fillLane(node, job, state);
    setPill(node.querySelector(".state-pill"), state);
  }

  function updateMini(node, machine) {
    var lanes = node.querySelector(".mline__lanes");
    var placed = placeJobs(machine, shownJobs(lanes));
    reconcile(
      lanes,
      placed.lanes,
      function (lane) { return String(lane.index); },
      createMiniLane,
      updateMiniLane
    );
    fillOnDeck(node.querySelector(".mline__next-text"), placed.upcoming, false);
  }

  // --- normal / detailed: one fixed card per machine --------------------------
  //
  //   RIG-A  (Running)  rig-a
  //   PROCESSING
  //   Series A                                        [hayai-nova-ppocr]
  //   Volume 02
  //   [==========-------------------------------------------------------]
  //   2 / 6 pages    33%    (host busy)                 done 14:05 (2s left)
  //   (detailed: the rate line and the stage table, a block of one height)
  //   ON DECK  Series B · Volume 02 · hayai-nova-ppocr
  //   (a row that cannot start here, or a blank line)

  function createMachine() {
    var node = el("article", "machine");
    var head = el("header", "machine__head");
    head.appendChild(el("span", "machine__name"));
    head.appendChild(el("span", "state-pill"));
    head.appendChild(el("span", "machine__label"));
    node.appendChild(head);
    node.appendChild(el("div", "machine__jobs"));
    var next = el("p", "machine__next");
    next.appendChild(el("span", "field__label", "On deck"));
    next.appendChild(el("span", "machine__next-text"));
    node.appendChild(next);
    node.appendChild(el("p", "machine__cannot"));
    return node;
  }

  // Rows whose runner keeps failing to start on this machine: the next try.
  function cannotStartLine(rows) {
    if (!rows || !rows.length) return "";
    return rows.map(function (row) {
      return row.generation + " cannot start here — next try " + clockTime(row.until) +
        (row.error ? " (" + row.error + ")" : "");
    }).join(" · ");
  }

  function updateMachine(node, machine) {
    setText(node.querySelector(".machine__name"), machine.name);
    var pill = node.querySelector(".state-pill");
    setPill(pill, machine.state || stateOf((machine.jobs || [])[0]));
    // Held because its archive downloads keep failing; the error is an
    // admin's only (the server sends it to nobody else).
    var held = machine.held;
    pill.title = held
      ? held.reason + (held.until ? " until " + clockTime(held.until) : "") +
        (held.error ? ": " + held.error : "")
      : "";
    // Admins only: the server sends `label` to nobody else.
    setText(
      node.querySelector(".machine__label"),
      machine.label && machine.label !== machine.name ? machine.label : ""
    );
    var jobs = node.querySelector(".machine__jobs");
    var placed = placeJobs(machine, shownJobs(jobs));
    reconcile(
      jobs,
      placed.lanes,
      function (lane) { return String(lane.index); },
      createLane,
      updateLane
    );
    // The volume on deck: submitted behind the one being read, so it has no
    // progress of its own worth a bar. Its own field, never a lane.
    fillOnDeck(node.querySelector(".machine__next-text"), placed.upcoming, true);
    // One line, always there, blank when nothing fails: a row's start
    // backoff begins and ends while the machine stays connected. The whole
    // sentence is in the tooltip when the line is too narrow for it.
    var cannot = node.querySelector(".machine__cannot");
    var line = cannotStartLine(machine.cannot_start);
    setText(cannot, line || HOLD);
    cannot.title = line;
  }

  // A lane: what the machine is processing. Every element made here stays
  // for the life of the card; `updateLane` only writes into them. The recipe
  // and the stage table are `detailed` fields (a level change rebuilds the
  // cards from nothing).
  function createLane() {
    var node = el("div", "lane");
    node.appendChild(el("div", "field__label lane__label"));
    var head = el("div", "job__head");
    var names = el("div", "job__names");
    names.appendChild(el("span", "job__series"));
    names.appendChild(el("span", "job__volume"));
    head.appendChild(names);
    var gen = el("span", "job__gen");
    gen.appendChild(el("span", "badge badge--gen"));
    if (level === "detailed") gen.appendChild(el("span", "job-recipe"));
    head.appendChild(gen);
    node.appendChild(head);
    var bar = el("div", "progress-bar");
    bar.appendChild(el("div", "progress-bar__fill"));
    node.appendChild(bar);
    // pages | percent | host busy | time: four fixed cells (styles.css).
    var meta = el("div", "job__meta");
    meta.appendChild(el("span", "job__pages"));
    meta.appendChild(el("span", "job__pct"));
    meta.appendChild(el("span", "job__busy badge badge--muted"));
    meta.appendChild(el("span", "job__eta"));
    node.appendChild(meta);
    if (level === "detailed") node.appendChild(createDetail());
    return node;
  }

  function updateLane(node, lane) {
    var job = lane.job;
    var state = laneState(lane.machine, job);
    setText(
      node.querySelector(".lane__label"),
      lane.many ? "Processing · slot " + (lane.index + 1) : "Processing"
    );
    setText(node.querySelector(".job__series"), (job && job.series) || HOLD);
    var volume = node.querySelector(".job__volume");
    var empty = job ? "" : emptyLaneText(state, false, lane.machine);
    setText(volume, job ? job.volume || HOLD : empty);
    // A narrow card cuts the line; the whole of it is on hover.
    setAttr(volume, "title", empty || null);
    setText(node.querySelector(".job__gen .badge--gen"), (job && job.generation) || "");
    var recipe = node.querySelector(".job-recipe");
    if (recipe) setText(recipe, job ? recipeText(job) : "");
    fillLane(node, job, state);
    // Something else is loading this machine right now: its pages come slower
    // than the machine can read them, and that is not learned as its speed.
    var busy = node.querySelector(".job__busy");
    var isBusy = !!(job && job.host_busy);
    setText(busy, isBusy ? "host busy" : "");
    busy.title = isBusy ? "another program is using this machine's CPU" : "";
    var detail = node.querySelector(".job__detail");
    if (detail) updateDetail(detail, job);
  }

  // engine . detector, only at `detailed` (the server sends them nowhere
  // else) and only when it says more than the generation's name does.
  function recipeText(job) {
    var engine = job.engine || "";
    if (!engine) return "";
    var detector = job.detector && job.detector !== engine ? job.detector : "";
    var recipe = detector ? engine + " · " + detector : engine;
    return recipe === job.generation ? "" : recipe;
  }

  // detailed: what this machine has really delivered on this layer (pages
  // over the wall seconds of its finished volumes -- never the ETA model's
  // fitted rate), then the model's fixed costs.
  function rateLine(job) {
    var parts = [];
    var real = ppm(job.throughput_pages_per_minute);
    if (real) parts.push("~" + real + " pages/min here");
    if (typeof job.latency_seconds === "number" && job.latency_seconds > 0) {
      parts.push("latency " + job.latency_seconds.toFixed(1) + " s a volume");
    }
    if (typeof job.startup_seconds === "number" && job.startup_seconds > 0) {
      parts.push("startup " + (job.startup_rough ? "≈ " : "") + formatDuration(job.startup_seconds));
    }
    return parts.join(" · ");
  }

  function renderMachineCards(machines) {
    machines = inPlace(machines);
    var minimal = level === "minimal";
    reconcile(
      machinesList(),
      machines,
      function (machine) { return machine.name; },
      minimal ? createMini : createMachine,
      minimal ? updateMini : updateMachine
    );
    return machines.length;
  }

  // The Machines section: a card for every machine connected, whatever it is
  // doing. With none, the section says so in their place -- the queue's hold
  // ("No processor connected since ...") when that is why, or a plain "No
  // machine connected". Either comes and goes only with a machine connecting
  // or leaving, never between jobs.
  function renderMachines(data) {
    var count = renderMachineCards(data.machines || []);
    var hold = count ? null : data.processing_hold || null;
    renderProcessingHold(hold);
    setHidden($machinesNone, count > 0 || !!hold);
  }

  // --- speed (detailed only) ------------------------------------------------
  //
  // One line per layer being read: what the machines reading it deliver
  // together, as real throughput. Which machine does what is the admin
  // panel's business; minimal and normal are sent no speed at all.

  function speedText(entry) {
    var n = entry.machines || 0;
    return "~" + ppm(entry.pages_per_minute) + " pages/min" +
      (n > 1 ? " across " + n + " machines" : "");
  }

  // The section sits between the cards and the pending list, so it is shown
  // (at `detailed`) whenever a machine is connected and is ONE height for as
  // long as the same machines are: one line for every lane there is (the
  // most layers they can be reading at once), whatever the server has to
  // report this second. Between two volumes it reports nothing for their
  // layer -- a volume still starting has delivered no pages -- so a layer a
  // machine is still holding keeps its last known line, and in the row it
  // had. A row with nothing to say says so with a dash.
  var speedRows = [];
  var speedKnown = {};

  function renderSpeed(data) {
    var machines = data.machines || [];
    var show = level === "detailed" && machines.length > 0;
    setHidden($speedSection, !show);
    var lines = 0;
    var reading = {};
    machines.forEach(function (machine) {
      lines += laneCount(machine);
      (machine.jobs || []).forEach(function (job) { reading[job.generation || ""] = true; });
    });
    var known = {};
    if (show) {
      (data.speed || []).forEach(function (entry) {
        if (ppm(entry.pages_per_minute)) known[entry.generation || ""] = entry;
      });
      Object.keys(speedKnown).forEach(function (name) {
        if (!(name in known) && reading[name]) known[name] = speedKnown[name];
      });
    } else {
      lines = 0;
    }
    speedKnown = known;
    var rows = [];
    for (var i = 0; i < lines; i++) {
      var had = speedRows[i];
      rows.push(had != null && had in known ? had : null);
    }
    Object.keys(known).forEach(function (name) {
      if (rows.indexOf(name) >= 0) return;
      var free = rows.indexOf(null);
      if (free >= 0) rows[free] = name;
    });
    speedRows = rows;
    reconcile(
      $speedList,
      rows.map(function (name) { return name == null ? null : known[name]; }),
      function (entry, index) { return String(index); },
      function () {
        var node = el("li", "speed__item");
        node.appendChild(el("span", "speed__gen badge badge--gen"));
        node.appendChild(el("span", "speed__text"));
        return node;
      },
      function (node, entry) {
        setText(node.querySelector(".speed__gen"), entry ? entry.generation || "" : "");
        setText(node.querySelector(".speed__text"), entry ? speedText(entry) : NONE);
        setAttr(node, "data-none", entry ? null : "");
      }
    );
  }

  // --- banners, run order ----------------------------------------------------

  // `hold` is `{reason: "no-processor", since, last?: {name, disconnected_at}}`
  // while this server does no OCR of its own and no processor is logged in.
  function renderProcessingHold(hold) {
    if (!hold) {
      setHidden($processingHold, true);
      setText($processingHoldText, "");
      return;
    }
    var last = hold.last
      ? " — last: " + hold.last.name + ", disconnected " + clockTime(hold.last.disconnected_at)
      : "";
    setText(
      $processingHoldText,
      "No processor connected since " + clockTime(hold.since) + last +
      ". The queue is holding until one logs in."
    );
    setHidden($processingHold, false);
  }

  // Generations the queue is holding because no connected machine can run
  // them (a forced precision nobody's card supports), one plain line each:
  // "paddle-manga: No connected machine can run bf16". Sent to admins only;
  // the list is hidden, taking no room, while there is none.
  function renderHeldRows(rows) {
    var lines = (Array.isArray(rows) ? rows : [])
      .filter(function (row) { return row && row.generation && row.reason; })
      .map(function (row) { return row.generation + ": " + row.reason; });
    var current = Array.prototype.map.call($heldRows.children, function (li) {
      return li.textContent;
    });
    if (current.join("\n") !== lines.join("\n")) {
      $heldRows.innerHTML = "";
      lines.forEach(function (line) { $heldRows.appendChild(el("li", "held-rows__item", line)); });
    }
    setHidden($heldRows, lines.length === 0);
  }

  // The configured generations, in the order the worker runs them.
  function renderRunOrder(generations) {
    var names = (generations || [])
      .map(function (gen) { return gen.name || ""; })
      .filter(Boolean);
    setHidden($runOrder, !names.length);
    var html = names
      .map(function (name, i) {
        return (
          '<li class="queue-order__item">' +
          '<span class="queue-order__pos">' + (i + 1) + "</span>" +
          escapeHtml(name) +
          "</li>"
        );
      })
      .join("");
    if ($runOrderList.dataset.html !== html) {
      $runOrderList.dataset.html = html;
      $runOrderList.innerHTML = html;
    }
  }

  // --- pending -------------------------------------------------------------

  // A pending row's layer badge (and, at `detailed`, its recipe).
  function fillGen(node, item) {
    var name = (item.generation || "") + "|" + recipeText(item);
    if (node.dataset.name === name) return;
    node.dataset.name = name;
    node.innerHTML = "";
    var badge = genBadge(item.generation);
    if (badge) node.appendChild(badge);
    var recipe = recipeText(item);
    if (recipe) node.appendChild(el("span", "job-recipe", recipe));
  }

  // A pending volume's own finishing time: "~14:07", one mark for every
  // prediction. One made from a guessed length (the median of the queue)
  // rather than the volume's own page count is drawn quieter, with a dotted
  // underline, and its tooltip says so -- two marks ("~" and "≈") read as
  // two different kinds of time.
  function updatePendingEta(node, item) {
    var at = localClock(item.eta_at);
    var cls =
      "pending-list__eta" +
      (!at ? " pending-list__eta--unknown" : item.rough ? " pending-list__eta--rough" : "");
    if (node.className !== cls) node.className = cls;
    node.title = !at
      ? item.reason || "no estimate yet"
      : item.rough
        ? "rough: this volume's length is not known yet, so the median length of the queued volumes stands in"
        : "estimated from this generation's measured rate";
    setText(node, !at ? "—" : "~" + at);
  }

  function createPending() {
    var node = el("li", "pending-list__item");
    node.appendChild(el("span", "pending-list__position"));
    node.appendChild(el("span", "pending-list__series"));
    node.appendChild(el("span", "pending-list__volume"));
    node.appendChild(el("span", "pending-list__gen"));
    node.appendChild(el("span", "pending-list__attempt badge badge--muted"));
    node.appendChild(el("span", "pending-list__eta"));
    node.appendChild(el("span", "pending-list__returned"));
    return node;
  }

  // A volume a processor gave back because it could not download it: who,
  // and why. A visitor is sent the alias and the category; an admin, the
  // real name and the error.
  function returnedLine(returned) {
    if (!returned) return "";
    return "returned by " + (returned.machine || "a machine") + ": " +
      (returned.error ? returned.class + ": " + returned.error : returned.reason || "");
  }

  function updatePending(node, item, index) {
    setText(node.querySelector(".pending-list__position"), String(index + 1));
    setText(node.querySelector(".pending-list__series"), item.series || "");
    setText(node.querySelector(".pending-list__volume"), item.volume || "");
    fillGen(node.querySelector(".pending-list__gen"), item);
    var attempt = node.querySelector(".pending-list__attempt");
    setText(attempt, item.attempts ? "attempt " + (item.attempts + 1) : "");
    setHidden(attempt, !item.attempts);
    updatePendingEta(node.querySelector(".pending-list__eta"), item);
    var returned = node.querySelector(".pending-list__returned");
    var line = returnedLine(item.returned);
    setText(returned, line);
    setHidden(returned, !line);
  }

  // minimal: one compact line a volume -- "Series · Volume  [layer]  ~14:07".
  function createCompact() {
    var node = el("li", "pc");
    node.appendChild(el("span", "pc__what"));
    node.appendChild(el("span", "pc__gen"));
    node.appendChild(el("span", "pending-list__eta"));
    return node;
  }

  function updateCompact(node, item) {
    setText(
      node.querySelector(".pc__what"),
      (item.series ? item.series + " · " : "") + (item.volume || "")
    );
    setText(node.querySelector(".pc__gen"), item.generation || "");
    updatePendingEta(node.querySelector(".pending-list__eta"), item);
  }

  // `pending` is the OCR worker's own queue, already in processing order.
  // Rendered as received: never sorted, grouped or de-duplicated here. At
  // `minimal` the server sends only the first few, and the count.
  function renderPending(data) {
    var list = data.pending || [];
    var count = typeof data.pending_count === "number" ? data.pending_count : list.length;
    var compact = level === "minimal";
    setText($pendingOcrCount, String(count));
    setHidden($pendingOcrEmpty, list.length > 0);
    setHidden($pendingOcrList, !list.length);
    setHidden($pendingOcrOrder, compact || !list.length);
    setText($pendingOcrOrder, pendingOrderSentence(data.generations || []));
    $pendingOcrList.classList.toggle("pending-compact", compact);
    reconcile(
      $pendingOcrList,
      list,
      jobKey,
      compact ? createCompact : createPending,
      compact ? updateCompact : updatePending
    );
    var more = count - list.length;
    setHidden($pendingMore, !compact || more <= 0);
    setText($pendingMore, more > 0 ? "+" + more + " more" : "");
  }

  function renderQueueDone(doneAt) {
    var at = localClock(doneAt);
    setHidden($queueDone, !at);
    setText($queueDone, at ? "everything done by " + at : "");
  }

  function pendingOrderSentence(generations) {
    return (
      "Next job first." +
      (generations.length > 1 ? " Generations run in their configured order;" : "") +
      " series take turns, each in reading order."
    );
  }

  function renderPendingThumbs(count) {
    if (count == null) {
      setHidden($thumbSection, true);
      return;
    }
    setHidden($thumbSection, false);
    setText($pendingThumbCount, String(count));
    setText(
      $pendingThumbText,
      count === 0
        ? "No volumes waiting for thumbnails"
        : count + " volume" + (count !== 1 ? "s" : "") + " waiting for thumbnail generation"
    );
  }

  // Failed volumes. A visitor is told the generic reason the server chose
  // ("engine error", "archive incomplete"); an admin is sent the error and
  // the log path as well, and only an admin.
  function renderFailed(list) {
    list = list || [];
    setHidden($failedSection, !list.length);
    setText($failedCount, String(list.length));
    var html = list.length
      ? '<ul class="pending-list__items">' + list.map(failedHtml).join("") + "</ul>"
      : "";
    if ($failedList.dataset.html !== html) {
      $failedList.dataset.html = html;
      $failedList.innerHTML = html;
    }
  }

  function failedHtml(item) {
    var attempts = item.attempts || 1;
    return (
      '<li class="pending-list__item pending-list__item--failed">' +
      '<div class="failed-item__header">' +
      '<span class="pending-list__series">' + escapeHtml(item.series || "") + "</span>" +
      '<span class="pending-list__volume">' + escapeHtml(item.volume || "") + "</span>" +
      '<span class="pending-list__gen">' + (item.generation
        ? '<span class="badge badge--gen">' + escapeHtml(item.generation) + "</span>"
        : "") + "</span>" +
      '<span class="badge badge--error">' + attempts + " attempt" + (attempts !== 1 ? "s" : "") +
      "</span>" +
      "</div>" +
      '<div class="failed-item__reason">' + escapeHtml(item.reason || "failed — will retry") +
      "</div>" +
      (item.error ? '<div class="failed-item__error">' + escapeHtml(item.error) + "</div>" : "") +
      (item.log_file
        ? '<div class="failed-item__log">Log: <code>' + escapeHtml(item.log_file) + "</code></div>"
        : "") +
      "</li>"
    );
  }

  // Volumes the extra generations step over. Quieter than Failed on purpose:
  // nothing went wrong and nothing is being retried.
  function renderSkipped(list) {
    list = list || [];
    setHidden($skippedSection, !list.length);
    setText($skippedCount, String(list.length));
    var html = list.length
      ? '<ul class="pending-list__items">' + list.map(skippedHtml).join("") + "</ul>"
      : "";
    if ($skippedList.dataset.html !== html) {
      $skippedList.dataset.html = html;
      $skippedList.innerHTML = html;
    }
  }

  function skippedHtml(item) {
    var gens = (item.generations || []).filter(Boolean);
    return (
      '<li class="pending-list__item pending-list__item--skipped">' +
      '<div class="skipped-item__header">' +
      '<span class="pending-list__series">' + escapeHtml(item.series || "") + "</span>" +
      '<span class="pending-list__volume">' + escapeHtml(item.volume || "") + "</span>" +
      '<span class="badge badge--muted">' + escapeHtml(missingPagesText(item)) + "</span>" +
      "</div>" +
      '<div class="skipped-item__gens">' +
      (gens.length
        ? "Not generated: " +
          gens.map(function (name) {
            return '<span class="badge badge--muted">' + escapeHtml(name) + "</span>";
          }).join(" ")
        : "Only the OCR it arrived with is kept.") +
      "</div>" +
      "</li>"
    );
  }

  function missingPagesText(item) {
    var missing = item.missing_pages;
    if (typeof missing !== "number" || missing <= 0) return "pages missing";
    var text = missing + " page" + (missing === 1 ? "" : "s") + " short";
    if (typeof item.page_count === "number" && item.page_count > 0) {
      text += " of " + item.page_count;
    }
    return text;
  }

  // --- the stage pipeline (detailed only) -------------------------------------
  //
  // One row a stage: a stage waiting on a FULL output queue is blocked by
  // what comes after it; one waiting on an EMPTY input queue is starved by
  // what comes before it.
  //
  // The block has ONE shape for the life of its card: the rate line, the
  // verdict line, STAGE_ROWS stage rows and the legend, all made once. A lane
  // with nothing being read keeps them all -- the rate a dash, the verdict
  // saying where the timings will show, the rows blank -- and a pipeline
  // shorter than the table leaves its last rows blank. The longest road the
  // runner has today is three stages; a longer one would add rows (inside
  // this block's fixed height, which scrolls) rather than hide a stage.
  var STAGE_ROWS = 3;

  var STAGE_KEYS = [
    ["busy", "busy"],
    ["blocked", "blocked by the next stage"],
    ["starved", "starved by the one before"],
    ["queue", "“5 of 8”: pages waiting for the next stage, of the 8 that fit"],
  ];

  function createDetail() {
    var node = el("div", "job__detail");
    node.appendChild(el("p", "job__rate"));
    var stages = el("div", "stages");
    stages.appendChild(el("p", "stages__verdict"));
    var list = el("ul", "stages__list");
    for (var i = 0; i < STAGE_ROWS; i++) list.appendChild(createStageRow());
    stages.appendChild(list);
    var legend = el("p", "stages__legend");
    STAGE_KEYS.forEach(function (key) {
      legend.appendChild(el("span", "stages__key stages__key--" + key[0], key[1]));
    });
    stages.appendChild(legend);
    node.appendChild(stages);
    return node;
  }

  function createStageRow() {
    var row = el("li", "stage");
    row.appendChild(el("span", "stage__name"));
    row.appendChild(el("span", "stage__device"));
    row.appendChild(el("span", "stage__width"));
    var bar = el("span", "stage__bar");
    bar.setAttribute("role", "img");
    bar.appendChild(el("span", "stage__seg stage__seg--busy"));
    bar.appendChild(el("span", "stage__seg stage__seg--blocked"));
    bar.appendChild(el("span", "stage__seg stage__seg--starved"));
    row.appendChild(bar);
    row.appendChild(el("span", "stage__busy"));
    row.appendChild(el("span", "stage__queue"));
    return row;
  }

  function updateDetail(detail, job) {
    var rate = job ? rateLine(job) : "";
    var rateNode = detail.querySelector(".job__rate");
    setText(rateNode, rate || HOLD);
    rateNode.title = rate;
    var pipeline = job && job.pipeline && job.pipeline.stages && job.pipeline.stages.length
      ? job.pipeline
      : null;
    var verdict = !job
      ? "Stage timings show here while a volume is read."
      : !pipeline
        ? "No stage timings for this volume yet."
        : pipeline.verdict ||
          "No stage singled out so far" +
          (pipeline.bottleneck ? " — " + pipeline.bottleneck + " is busiest" : "");
    var verdictNode = detail.querySelector(".stages__verdict");
    setText(verdictNode, verdict);
    verdictNode.title = verdict;
    setAttr(verdictNode, "data-none", pipeline && pipeline.verdict ? null : "");
    var stages = pipeline ? pipeline.stages : [];
    var list = detail.querySelector(".stages__list");
    while (list.children.length < stages.length) list.appendChild(createStageRow());
    var i = 0;
    for (var row = list.firstElementChild; row; row = row.nextElementSibling, i++) {
      var stage = stages[i] || null;
      updateStageRow(row, stage, !!stage && stage.key === pipeline.bottleneck);
    }
  }

  function isGpuDevice(device) {
    return String(device || "").indexOf("gpu") === 0;
  }

  function deviceLabel(device) {
    var text = String(device || "cpu");
    if (text === "cpu") return "CPU";
    if (text === "gpu") return "GPU";
    return isGpuDevice(text) ? "GPU " + text.slice(4) : text;
  }

  function updateStageRow(row, stage, isBottleneck) {
    setAttr(row, "data-worst", isBottleneck ? "" : null);
    setAttr(row, "data-empty", stage ? null : "");
    var device = row.querySelector(".stage__device");
    var bar = row.querySelector(".stage__bar");
    var segs = bar.children;
    if (!stage) {
      setText(row.querySelector(".stage__name"), "");
      setText(device, "");
      setAttr(device, "data-device", null);
      setText(row.querySelector(".stage__width"), "");
      setAttr(bar, "aria-label", null);
      setAttr(bar, "title", null);
      setAttr(bar, "tabindex", null);
      for (var j = 0; j < segs.length; j++) segs[j].style.width = "0%";
      setText(row.querySelector(".stage__busy"), "");
      setText(row.querySelector(".stage__queue"), "");
      return;
    }
    var busy = clampPct(stage.busy_pct);
    var blocked = clampPct(stage.blocked_pct);
    var starved = clampPct(stage.starved_pct);
    var blockedWidth = Math.min(blocked, 100 - busy);
    var starvedWidth = Math.min(starved, 100 - busy - blockedWidth);
    var q = stage.queue;
    setText(row.querySelector(".stage__name"), stage.key || "");
    setText(device, deviceLabel(stage.device));
    setAttr(device, "data-device", isGpuDevice(stage.device) ? "gpu" : "cpu");
    setText(row.querySelector(".stage__width"), stage.fused ? "fused" : "×" + (stage.workers || 1));
    setAttr(
      bar,
      "aria-label",
      stage.key + ": busy " + Math.round(busy) + "%, blocked " + Math.round(blocked) +
        "%, starved " + Math.round(starved) + "%"
    );
    setAttr(bar, "title", stageTitle(stage, busy, blocked, starved));
    setAttr(bar, "tabindex", "0");
    // Laid over each other from the left edge (styles.css): each segment's
    // width is where its share ENDS.
    segs[0].style.width = busy + "%";
    segs[1].style.width = busy + blockedWidth + "%";
    segs[2].style.width = busy + blockedWidth + starvedWidth + "%";
    setText(row.querySelector(".stage__busy"), Math.round(busy) + "%");
    setText(
      row.querySelector(".stage__queue"),
      q && typeof q.mean_depth === "number" ? q.mean_depth.toFixed(1) + " of " + q.capacity : ""
    );
  }

  function stageTitle(stage, busy, blocked, starved) {
    var parts = [stage.name || stage.key, "busy " + busy.toFixed(1) + "%"];
    if (stage.blocked_pct != null) parts.push("blocked " + blocked.toFixed(1) + "%");
    if (stage.starved_pct != null) parts.push("starved " + starved.toFixed(1) + "%");
    if (stage.queue && typeof stage.queue.mean_depth === "number") {
      parts.push(
        "queue " + stage.queue.name + ": mean depth " + stage.queue.mean_depth.toFixed(2) +
        " of " + stage.queue.capacity + ", peak " + stage.queue.max_depth
      );
    }
    return parts.join(" · ");
  }

  function clampPct(value) {
    if (typeof value !== "number" || !isFinite(value) || value < 0) return 0;
    return Math.min(100, value);
  }

  function escapeHtml(s) {
    var d = document.createElement("div");
    d.textContent = s;
    return d.innerHTML;
  }

  // The backend as it is written, not as the config spells it.
  var BACKEND_LABEL = { rocm: "ROCm", cuda: "CUDA", cpu: "CPU" };

  function backendLabel(name) {
    if (!name) return "";
    return BACKEND_LABEL[String(name).toLowerCase()] || String(name);
  }

  // --- one payload -------------------------------------------------------------

  function render(data) {
    var newLevel = data.level || "normal";
    if (newLevel !== level) {
      // A level change re-draws from nothing: the shapes differ. It is the
      // only thing that ever empties the machines block.
      $machines.innerHTML = "";
      $list = null;
      $pendingOcrList.innerHTML = "";
      live = [];
      level = newLevel;
      document.body.dataset.level = level;
    }
    setHidden($backendLine, !data.backend);
    setText($backend, backendLabel(data.backend));
    renderRunOrder(data.generations);
    renderMachines(data);
    renderHeldRows(data.held_rows);
    renderSpeed(data);
    renderPending(data);
    renderQueueDone(data.queue_done_at || null);
    renderPendingThumbs(
      typeof data.pending_thumbnails === "number" ? data.pending_thumbnails : null
    );
    renderFailed(data.failed);
    renderSkipped(data.skipped_missing_pages);
    document.body.classList.add("queue-ready");
  }

  function schedule() {
    clearTimeout(pollTimer);
    var delay = document.hidden
      ? HIDDEN_INTERVAL
      : failures
        ? Math.min(FALLBACK_INTERVAL * Math.pow(2, failures - 1), MAX_BACKOFF)
        : FAST_INTERVAL;
    pollTimer = setTimeout(poll, delay);
  }

  function poll() {
    clearTimeout(pollTimer);
    var headers = getStatusHeaders();
    if (etag) headers["If-None-Match"] = etag;
    // `no-store`: the browser's own cache stays out of it, so a 304 reaches
    // this code as a 304 and costs no parse and no render.
    fetch("/queue/api/status", { headers: headers, cache: "no-store" })
      .then(function (r) {
        if (r.status === 401) {
          window.location.href = "/login";
          return null;
        }
        // The stored login no longer works (a changed password, a removed
        // account): the server answered as it answers any visitor, and says
        // so. Drop it, so the next poll does not send it again.
        if (r.headers.get("X-Queue-Auth") === "failed") forgetStoredLogin();
        if (r.status === 304) return null;
        if (!r.ok) throw new Error("status " + r.status);
        etag = r.headers.get("ETag");
        return r.json();
      })
      .then(function (data) {
        failures = 0;
        if (data) render(data);
      })
      .catch(function () {
        failures += 1;
      })
      .then(schedule);
  }

  document.addEventListener("visibilitychange", function () {
    if (!document.hidden) poll();
  });

  function init() {
    setInterval(tick, TICK_MS);
    fetch("/queue/api/config")
      .then(function (r) { return r.json(); })
      .then(function (cfg) {
        queueConfig = cfg || queueConfig;
        return updateNav();
      })
      .then(function () {
        if (!queueConfig.public_access && !getSessionAuth()) {
          window.location.href = "/login";
          return;
        }
        poll();
      })
      .catch(function () {
        updateNav();
        poll();
      });
  }

  window.logout = logout;
  init();
})();
