// Mokuro Bunko desktop app: shared helpers for the /app pages (plain JS, no build).
// The pages are served by this machine's local control listener; the browser is
// signed in by a cookie (/app/login), so fetch() needs no token. POSTs carry JSON.
(function () {
  'use strict';

  function esc(s) {
    return String(s == null ? '' : s)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }

  async function api(method, path, body) {
    const opts = { method: method, headers: {} };
    if (body !== undefined) {
      opts.headers['Content-Type'] = 'application/json';
      opts.body = JSON.stringify(body);
    }
    let resp;
    try {
      resp = await fetch(path, opts);
    } catch (e) {
      throw new Error('This app is not running any more. Start it again from the tray or with `mokuro-bunko gui`.');
    }
    let data = {};
    try { data = await resp.json(); } catch (_) { data = {}; }
    if (!resp.ok) {
      const err = new Error(data.error || ('HTTP ' + resp.status));
      err.status = resp.status;
      err.data = data;
      throw err;
    }
    return data;
  }

  const get = (p) => api('GET', p);
  const post = (p, b) => api('POST', p, b === undefined ? {} : b);

  let infoCache = null;
  async function info(force) {
    if (!infoCache || force) infoCache = await get('/app/api/info');
    return infoCache;
  }

  function toast(message, kind) {
    let box = document.querySelector('.toast-container');
    if (!box) {
      box = document.createElement('div');
      box.className = 'toast-container';
      document.body.appendChild(box);
    }
    const t = document.createElement('div');
    t.className = 'toast toast--' + (kind || 'success');
    t.setAttribute('role', 'status');
    t.textContent = message;
    box.appendChild(t);
    requestAnimationFrame(() => t.classList.add('show'));
    setTimeout(() => { t.classList.remove('show'); setTimeout(() => t.remove(), 300); }, 4000);
  }

  const LOGO = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M8 12h8"/><path d="M12 8v8"/></svg>';

  // The header: brand + Home / Setup / Settings / Dashboard (+ the library, when known).
  async function header(current) {
    const el = document.getElementById('app-header');
    if (!el) return;
    const links = [
      ['home', 'Home', '/app/'],
      ['setup', 'Setup', '/app/setup'],
      ['settings', 'Settings', '/app/settings'],
      ['dashboard', 'Dashboard', '/app/dashboard'],
    ];
    const nav = links.map(([k, label, href]) =>
      '<a href="' + href + '" class="btn ' + (k === current ? 'btn--secondary' : 'btn--ghost') + ' btn--sm"' +
      (k === current ? ' aria-current="page"' : '') + '>' + label + '</a>').join('');
    el.innerHTML =
      '<a href="/app/" class="mokuro-header__brand"><div class="mokuro-header__logo">' + LOGO + '</div>' +
      '<span class="mokuro-header__title">Mokuro Bunko</span><span class="app-role" id="app-role"></span></a>' +
      '<nav class="mokuro-header__nav" aria-label="App">' + nav + '<span id="app-library-link"></span></nav>';
    try {
      const i = await info();
      const role = { gui: 'setup', server: 'library server', processor: 'processor' }[i.role] || i.role;
      document.getElementById('app-role').textContent = role;
      if (i.library_url && i.config_exists) {
        document.getElementById('app-library-link').innerHTML =
          '<a href="' + esc(i.library_url) + '" target="_blank" rel="noopener" class="btn btn--ghost btn--sm">Library &#8599;</a>';
      }
    } catch (_) { /* the page shows its own error */ }
  }

  // Keep the `gui` command from closing while a page is open.
  function heartbeat() {
    setInterval(() => { fetch('/app/api/ping').catch(() => {}); }, 60000);
  }

  function setBusy(btn, busy, label) {
    if (!btn) return;
    if (busy) {
      btn.dataset.label = btn.textContent;
      btn.disabled = true;
      btn.textContent = label || 'Working…';
    } else {
      btn.disabled = false;
      if (btn.dataset.label) btn.textContent = btn.dataset.label;
    }
  }

  function showError(el, err) {
    if (!el) return;
    el.textContent = err ? (err.message || String(err)) : '';
    el.hidden = !err;
  }

  // Run a CLI job and stream its output into `box` (an element made by jobBox()).
  // Resolves with the final summary.
  function runJob(request, box) {
    return new Promise(async (resolve, reject) => {
      let job;
      try {
        job = await post('/app/api/jobs', request);
      } catch (e) {
        reject(e);
        return;
      }
      attachJob(job, box).then(resolve, reject);
    });
  }

  function jobBox(container) {
    container.innerHTML =
      '<div class="job">' +
      '<div class="job__head"><span class="job__title"></span><span class="badge job__state"></span></div>' +
      '<code class="job__cmd"></code>' +
      '<div class="progress" role="progressbar" aria-valuemin="0" aria-valuemax="100"><div class="progress__fill"></div></div>' +
      '<p class="job__stage"></p>' +
      '<pre class="job__log" tabindex="0" aria-live="polite"></pre>' +
      '<div class="job__actions"><button type="button" class="btn btn--secondary btn--sm job__cancel">Cancel</button></div>' +
      '</div>';
    return container.querySelector('.job');
  }

  function renderState(box, s) {
    const badge = box.querySelector('.job__state');
    const map = { running: ['Running', 'badge--info'], ok: ['Done', 'badge--success'],
      failed: ['Failed', 'badge--error'], cancelled: ['Cancelled', 'badge--warning'] };
    const [label, cls] = map[s.state] || [s.state, 'badge--muted'];
    badge.textContent = label + (s.state === 'failed' && s.exit_code != null ? ' (exit ' + s.exit_code + ')' : '');
    badge.className = 'badge job__state ' + cls;
    box.querySelector('.job__cancel').hidden = s.state !== 'running';
    box.dataset.state = s.state;
  }

  function renderProgress(box, progress, stage) {
    const bar = box.querySelector('.progress');
    const fill = box.querySelector('.progress__fill');
    if (progress == null) {
      bar.classList.add('progress--indeterminate');
      bar.removeAttribute('aria-valuenow');
    } else {
      bar.classList.remove('progress--indeterminate');
      fill.style.width = Math.max(0, Math.min(100, progress)) + '%';
      bar.setAttribute('aria-valuenow', String(Math.round(progress)));
    }
    if (stage !== undefined) box.querySelector('.job__stage').textContent = stage || '';
  }

  function attachJob(job, box) {
    return new Promise((resolve) => {
      box.querySelector('.job__title').textContent = job.title;
      box.querySelector('.job__cmd').textContent = job.command;
      renderState(box, job);
      renderProgress(box, job.progress, job.stage);
      const log = box.querySelector('.job__log');
      log.textContent = '';
      box.querySelector('.job__cancel').onclick = () => post('/app/api/jobs/' + job.id + '/cancel').catch(() => {});
      const es = new EventSource('/app/api/jobs/' + job.id + '/events');
      es.addEventListener('line', (ev) => {
        const d = JSON.parse(ev.data);
        const atEnd = log.scrollTop + log.clientHeight >= log.scrollHeight - 4;
        log.textContent += (log.textContent ? '\n' : '') + d.text;
        if (atEnd) log.scrollTop = log.scrollHeight;
      });
      es.addEventListener('progress', (ev) => {
        const d = JSON.parse(ev.data);
        renderProgress(box, d.progress, d.stage);
      });
      es.addEventListener('done', (ev) => {
        const d = JSON.parse(ev.data);
        es.close();
        renderState(box, d);
        renderProgress(box, d.state === 'ok' ? 100 : d.progress, d.stage);
        resolve(d);
      });
      es.onerror = () => {
        // The stream ends after `done`; anything else: read the final state.
        if (box.dataset.state === 'running') {
          setTimeout(async () => {
            try {
              const s = await get('/app/api/jobs/' + job.id);
              if (s.state !== 'running') {
                es.close();
                renderState(box, s);
                resolve(s);
              }
            } catch (_) { es.close(); resolve({ state: 'failed', id: job.id }); }
          }, 1500);
        }
      };
    });
  }

  // A folder (or file) picker over /app/api/fs. Resolves with the chosen path or null.
  function pick(opts) {
    opts = opts || {};
    return new Promise((resolve) => {
      const overlay = document.createElement('div');
      overlay.className = 'modal-overlay open';
      overlay.innerHTML =
        '<div class="modal picker" role="dialog" aria-modal="true" aria-labelledby="picker-title">' +
        '<div class="modal__header"><h3 class="modal__title" id="picker-title">' + esc(opts.title || 'Choose a folder') + '</h3>' +
        '<button type="button" class="modal__close" aria-label="Close">&times;</button></div>' +
        '<div class="modal__body">' +
        '<div class="picker__path"><input type="text" class="form-input picker__input" aria-label="Path"><button type="button" class="btn btn--secondary btn--sm picker__go">Go</button></div>' +
        '<ul class="picker__list" role="listbox"></ul><p class="form-error picker__err" hidden></p>' +
        (opts.files ? '' : '<p class="form-hint">Open a folder, then choose it. A new folder is made when the setup saves.</p>') +
        '</div><div class="modal__footer">' +
        '<button type="button" class="btn btn--secondary picker__cancel">Cancel</button>' +
        (opts.files ? '' : '<button type="button" class="btn btn--primary picker__choose">Choose this folder</button>') +
        '</div></div>';
      document.body.appendChild(overlay);
      const input = overlay.querySelector('.picker__input');
      const list = overlay.querySelector('.picker__list');
      const err = overlay.querySelector('.picker__err');
      let current = null;
      function close(v) { overlay.remove(); resolve(v); }
      async function load(path) {
        try {
          const q = '/app/api/fs?path=' + encodeURIComponent(path || '') +
            (opts.files ? '&exts=' + encodeURIComponent(opts.files) : '');
          const d = await get(q);
          current = d;
          input.value = d.path;
          showError(err, null);
          const items = [];
          if (d.parent) items.push('<li><button type="button" class="picker__item" data-dir="' + esc(d.parent) + '">&#8617; ..</button></li>');
          d.dirs.forEach((n) => items.push('<li><button type="button" class="picker__item" data-dir="' + esc(d.path + d.sep + n) + '">&#128193; ' + esc(n) + '</button></li>'));
          d.files.forEach((n) => items.push('<li><button type="button" class="picker__item picker__file" data-file="' + esc(d.path + d.sep + n) + '">&#128196; ' + esc(n) + '</button></li>'));
          if (!d.dirs.length && !d.files.length) items.push('<li class="text-muted picker__empty">(empty)</li>');
          list.innerHTML = items.join('');
        } catch (e) { showError(err, e); }
      }
      list.addEventListener('click', (ev) => {
        const b = ev.target.closest('button');
        if (!b) return;
        if (b.dataset.dir) load(b.dataset.dir);
        else if (b.dataset.file) close(b.dataset.file);
      });
      overlay.querySelector('.picker__go').onclick = () => load(input.value);
      input.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') load(input.value); });
      overlay.querySelector('.modal__close').onclick = () => close(null);
      overlay.querySelector('.picker__cancel').onclick = () => close(null);
      const choose = overlay.querySelector('.picker__choose');
      if (choose) choose.onclick = () => close(current ? current.path : input.value || null);
      overlay.addEventListener('keydown', (ev) => { if (ev.key === 'Escape') close(null); });
      load(opts.start || '');
    });
  }

  // Wire every [data-pick] button: fills the input named by data-pick.
  function wirePickers(root) {
    (root || document).querySelectorAll('[data-pick]').forEach((b) => {
      b.addEventListener('click', async () => {
        const input = document.getElementById(b.dataset.pick);
        const v = await pick({ title: b.dataset.title, files: b.dataset.files, start: input.value });
        if (v) { input.value = v; input.dispatchEvent(new Event('change')); }
      });
    });
  }

  // Wizard steps: sections .wizard-step in order; dots in .wizard-dots.
  function wizard(steps, onShow) {
    let i = 0;
    const dots = document.querySelector('.wizard-dots');
    if (dots) dots.innerHTML = steps.map((_, n) => '<span class="wizard-dot" data-n="' + n + '"></span>').join('');
    function show(n) {
      i = Math.max(0, Math.min(steps.length - 1, n));
      steps.forEach((id, k) => {
        const el = document.getElementById(id);
        el.hidden = k !== i;
      });
      if (dots) dots.querySelectorAll('.wizard-dot').forEach((d, k) => d.classList.toggle('active', k <= i));
      const first = document.getElementById(steps[i]).querySelector('h2');
      if (first) { first.setAttribute('tabindex', '-1'); first.focus({ preventScroll: true }); }
      window.scrollTo(0, 0);
      if (onShow) onShow(steps[i]);
    }
    show(0);
    return { show: show, next: () => show(i + 1), prev: () => show(i - 1), get index() { return i; },
      go: (id) => show(steps.indexOf(id)) };
  }

  function fmtBytes(n) {
    if (n == null) return '-';
    const u = ['B', 'KB', 'MB', 'GB', 'TB'];
    let i = 0; let v = n;
    while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
    return (i ? v.toFixed(1) : v) + ' ' + u[i];
  }

  // The library's admin panel, at a tab (`#settings`, `#connectivity`, ...).
  async function adminLink(tab) {
    const i = await info();
    if (!i.library_url) return null;
    return i.library_url + (i.admin_path || '/_admin') + (tab ? '#' + tab : '');
  }

  // A default storage folder this user cannot write (e.g. a ~/.local owned by root,
  // `/app/api/info`'s *_storage_check): say so, put the writable alternative in the
  // field, and show the one-line fix that keeps the default.
  function storageNote(note, input, check) {
    if (!note || !check || check.writable !== false) return;
    if (input.value && input.value !== String(check.path)) return; // not the default
    let html = esc(check.problem) + ', so <code>' + esc(check.path) + '</code> cannot be used.';
    if (check.suggestion) {
      input.value = check.suggestion;
      html += ' The folder above is proposed instead (any folder you can write works).';
    } else {
      html += ' Choose a folder you can write.';
    }
    if (check.fix) {
      html += ' Or fix the permissions once in a terminal and keep the default: <code>' + esc(check.fix) + '</code>';
    }
    note.innerHTML = html;
    note.hidden = false;
  }

  // Shown where the CPU is picked for paddle-manga (setup, settings); the admin panel
  // gets the same text from the engine catalog (bunko_core::engines::PADDLE_CPU_NOTE).
  const PADDLE_CPU_NOTE = 'paddle-manga on the CPU is slow (about 40 s a page on 16 threads) and downloads 3.6 GB of weights; hayai-nova is the CPU engine of choice.';

  window.App = {
    esc, api, get, post, info, toast, header, heartbeat, setBusy, showError,
    runJob, jobBox, attachJob, pick, wirePickers, wizard, fmtBytes, adminLink,
    storageNote, PADDLE_CPU_NOTE,
  };

  document.addEventListener('DOMContentLoaded', () => {
    const h = document.getElementById('app-header');
    if (h) header(h.dataset.current);
    heartbeat();
  });
})();
