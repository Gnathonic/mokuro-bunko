// Dashboard: this instance's /control/status, live over /control/events (SSE), with
// polling as the fallback. Pause / resume through /control/pause and /control/resume.
(function () {
  'use strict';
  const { esc, get, post, toast, showError } = window.App;
  const $ = (id) => document.getElementById(id);
  const STATES = {
    working: 'Working', idle: 'Idle — waiting for volumes', paused: 'Paused', pausing: 'Pausing after this volume',
    connecting: 'Connecting', disconnected: 'Disconnected', error: 'Error', setup: 'Not set up',
  };
  let last = null;
  let pollTimer = null;
  let es = null;

  function dur(sec) {
    if (sec == null) return '—';
    sec = Math.round(sec);
    if (sec < 60) return sec + ' s';
    const m = Math.floor(sec / 60);
    if (m < 60) return m + ' min';
    return Math.floor(m / 60) + ' h ' + (m % 60) + ' min';
  }

  function num(n) { return n == null ? '—' : Number(n).toLocaleString(); }

  function tile(label, value, sub) {
    return '<div class="tile"><div class="tile__label">' + esc(label) + '</div><div class="tile__value">' + esc(value) +
      '</div>' + (sub ? '<div class="tile__sub">' + esc(sub) + '</div>' : '') + '</div>';
  }

  // `status.update`: the automatic update, as one line (null: nothing to say).
  function updateLine(u) {
    if (!u) return null;
    const v = u.version ? ' ' + u.version : '';
    switch (u.state) {
      case 'waiting':
        return 'Updating to' + v + '… waiting for a quiet moment' + (u.message ? ' (' + u.message + ')' : '') + '.';
      case 'downloading': case 'installing': case 'restarting':
        return 'Updating to' + v + '…' + (u.message ? ' ' + u.message : '');
      case 'updated': return 'Updated to' + v + (u.from ? ' (from ' + u.from + ')' : '') + '.';
      case 'failed': return 'Update to' + v + ' failed' + (u.message ? ': ' + u.message : '') + ' — will retry.';
      default: return null;
    }
  }

  // `status.install`: the background OCR backend install (0.7).
  function installText(i) {
    if (i.state === 'failed') return 'The OCR backend install failed' + (i.message ? ': ' + i.message : '') + '.';
    if (i.state === 'missing') return (i.message || 'No OCR backend is installed') + '.';
    if (i.state === 'done') return 'OCR backend installed' + (i.pack ? ' (' + i.pack + ')' : '') + '; local OCR uses it now.';
    const stage = {
      checking: 'Checking what this machine needs', waiting: 'Waiting for another OCR install on this machine',
      downloading: 'Downloading the ' + (i.variant ? i.variant + ' ' : '') + 'backend pack',
      unpacking: 'Unpacking and checking the backend pack', libraries: 'Fetching NVIDIA\'s CUDA libraries',
      installed: 'Backend pack installed', models: 'Fetching the OCR models',
    }[i.stage] || ('Installing (' + i.stage + ')');
    const bytes = i.done_bytes != null && i.total_bytes
      ? ' — ' + (i.done_bytes / 1e9).toFixed(2) + ' of ' + (i.total_bytes / 1e9).toFixed(2) + ' GB' : '';
    return stage + (i.percent != null ? ': ' + i.percent + '%' : '…') + bytes;
  }

  function renderInstall(i) {
    const panel = $('install-panel');
    panel.hidden = !i;
    if (!i) return;
    const running = i.state === 'running';
    const stuck = i.state === 'failed' || i.state === 'missing';
    $('install-text').textContent = installText(i);
    $('install-text').className = stuck ? 'problem problem--fail' : '';
    $('install-bar').hidden = !running;
    $('install-bar').classList.toggle('progress--indeterminate', running && i.percent == null);
    $('install-fill').style.width = (i.percent || 0) + '%';
    if (i.percent != null) $('install-bar').setAttribute('aria-valuenow', i.percent);
    $('install-hint').textContent = running
      ? 'It runs in the background: the server already serves, and you can close this page.' +
        (i.message && i.stage === 'models' ? ' (' + i.message + ')' : '')
      : (stuck ? (i.action || 'See the log for the details.') : '');
    $('install-actions').hidden = !stuck;
    $('install-retry').textContent = i.state === 'missing' ? 'Install' : 'Retry';
  }

  function render(s) {
    last = s;
    renderInstall(s.install);
    showError($('dash-error'), null);
    $('dash-name').textContent = (s.role === 'server' ? 'Library server' : s.role === 'processor' ? 'Processor' : 'This machine') +
      (s.name ? ' · ' + s.name : '');
    const pill = $('dash-state');
    pill.dataset.state = s.state;
    pill.textContent = STATES[s.state] || s.state;
    $('dash-setup').hidden = s.state !== 'setup';

    const paused = s.state === 'paused' || s.state === 'pausing';
    $('pause-controls').hidden = !s.can_pause;
    document.querySelectorAll('[data-pause]').forEach((b) => { b.hidden = paused; });
    $('resume').hidden = !paused;
    const p = s.pause || {};
    $('dash-pause').hidden = !p.mode;
    if (p.mode) {
      $('dash-pause').textContent = 'Paused ' + (p.mode === 'now' ? 'at once' : 'after the running volume') +
        (p.until ? ', until ' + new Date(p.until).toLocaleString() : ', until resumed') +
        (p.reason && p.reason !== 'user' ? ' (' + p.reason + ')' : '') +
        (p.since ? ' — since ' + new Date(p.since).toLocaleTimeString() : '') + '.';
    }

    const line = updateLine(s.update);
    $('dash-update').hidden = !line;
    if (line) {
      $('dash-update').textContent = line;
      $('dash-update').className = 'alert alert--' + (s.update.state === 'failed' ? 'warning' : 'info');
    }

    const st = s.stats || { today: {}, total: {} };
    const lib = s.library || {};
    $('tiles').innerHTML =
      tile('Today', num(st.today.volumes) + ' vol', num(st.today.pages) + ' pages · busy ' + dur(st.today.busy_seconds)) +
      tile('All time', num(st.total.volumes) + ' vol', num(st.total.pages) + ' pages') +
      tile('Speed', (st.rate_pages_per_minute || 0).toFixed(1), 'pages / minute') +
      (st.gpu_busy_percent != null ? tile('GPU busy', Math.round(st.gpu_busy_percent) + ' %', '') : '') +
      (st.cpu_cores_busy != null ? tile('CPU', st.cpu_cores_busy.toFixed(1), 'cores busy') : '') +
      (lib.queue_pending != null ? tile('Queue', num(lib.queue_pending), 'volumes waiting for OCR') : '');

    const cur = s.current || [];
    $('current').innerHTML = cur.length ? cur.map((v) => {
      const pct = v.pages_total ? Math.round(100 * v.pages_done / v.pages_total) : null;
      return '<div class="volume-row"><div class="volume-row__head"><span class="volume-row__name">' + esc(v.volume) + '</span>' +
        '<span>' + num(v.pages_done) + ' / ' + num(v.pages_total) + ' pages</span></div>' +
        '<div class="progress' + (pct == null ? ' progress--indeterminate' : '') + '" role="progressbar" aria-valuemin="0" aria-valuemax="100"' +
        (pct == null ? '' : ' aria-valuenow="' + pct + '"') + ' aria-label="' + esc(v.volume) + '"><div class="progress__fill" style="width:' + (pct || 0) + '%"></div></div>' +
        '<div class="volume-row__meta">' + [v.engine, v.precision, v.device,
          v.pages_per_second ? v.pages_per_second.toFixed(2) + ' pages/s' : null,
          v.eta_seconds != null ? 'about ' + dur(v.eta_seconds) + ' left' : null].filter(Boolean).map(esc).join(' · ') +
        '</div></div>';
    }).join('') : '<p class="text-muted">' + (paused ? 'Nothing — paused.' : 'Nothing.') + '</p>';

    // The install's own failure is in its panel above.
    const probs = (s.problems || []).filter((pr) => pr.kind !== 'ocr-install' || !s.install);
    $('problems-panel').hidden = !probs.length;
    $('problems').innerHTML = probs.map((pr) =>
      '<div class="problem problem--' + esc(pr.severity) + '">' + esc(pr.text) +
      (pr.hint ? '<div class="problem__hint">' + esc(pr.hint) + '</div>' : '') + '</div>').join('');

    const b = s.backend || {};
    $('details').innerHTML =
      '<dt>Library</dt><dd>' + (lib.url ? esc(lib.url) : '—') + ' ' +
        (lib.connected ? '<span class="badge badge--success">connected</span>' : '<span class="badge badge--muted">not connected</span>') +
        (lib.error ? '<br>' + esc(lib.error) : '') + '</dd>' +
      '<dt>OCR backend</dt><dd>' + esc(b.pack || 'none') + (b.devices && b.devices.length ? '<br>' + b.devices.map(esc).join('<br>') : '') + '</dd>' +
      '<dt>Version</dt><dd>' + esc(s.version) + (s.managed ? ' · started by the tray' : '') + '</dd>' +
      (s.urls && s.urls.logs_dir ? '<dt>Logs</dt><dd>' + esc(s.urls.logs_dir) + '</dd>' : '');
    const link = $('lib-link');
    const libUrl = (s.urls && s.urls.library) || lib.url;
    link.hidden = !libUrl;
    if (libUrl) link.href = libUrl;
  }

  async function poll() {
    try { render(await get('/control/status')); } catch (e) { showError($('dash-error'), e); }
  }

  function live(on) {
    $('dash-live').dataset.live = on ? '1' : '0';
    $('dash-live').textContent = on ? 'live' : 'refreshing every 2 s';
  }

  function startPolling() {
    if (pollTimer) return;
    live(false);
    pollTimer = setInterval(poll, 2000);
  }

  function connect() {
    if (!window.EventSource) { startPolling(); return; }
    es = new EventSource('/control/events');
    es.addEventListener('status', (ev) => {
      try { render(JSON.parse(ev.data)); } catch (_) { return; }
      live(true);
      if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
    });
    es.onerror = () => {
      // The browser reconnects by itself; poll meanwhile so the page stays current.
      startPolling();
    };
  }

  function morning() {
    const d = new Date();
    d.setHours(8, 0, 0, 0);
    if (d <= new Date()) d.setDate(d.getDate() + 1);
    return d;
  }

  async function pause(kind) {
    let body;
    if (kind === 'hour') body = { mode: 'after_volume', until: new Date(Date.now() + 3600e3).toISOString() };
    else if (kind === 'morning') body = { mode: 'after_volume', until: morning().toISOString() };
    else body = { mode: kind, until: null };
    try {
      const r = await post('/control/pause', body);
      if (r && r.state) render(r);
      else poll();
      toast(kind === 'now' ? 'Paused; the running volumes go back to the queue.' : 'Pausing after the running volume.');
    } catch (e) { toast(e.message, 'error'); }
  }

  async function resume() {
    try {
      const r = await post('/control/resume');
      if (r && r.state) render(r); else poll();
      toast('Resumed.');
    } catch (e) { toast(e.message, 'error'); }
  }

  document.addEventListener('DOMContentLoaded', () => {
    document.querySelectorAll('[data-pause]').forEach((b) => { b.onclick = () => pause(b.dataset.pause); });
    $('resume').onclick = resume;
    $('install-retry').onclick = async () => {
      $('install-retry').disabled = true;
      try {
        const r = await post('/control/ocr-install');
        if (r && r.status) render(r.status); else poll();
        toast(r && r.installing ? 'Installing the OCR backend in the background.' : 'Nothing to install.');
      } catch (e) { toast(e.message, 'error'); } finally { $('install-retry').disabled = false; }
    };
    poll();
    connect();
  });
})();
