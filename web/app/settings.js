// Settings: one page, the section chosen by the path (/app/settings/<section>).
(function () {
  'use strict';
  const { esc, get, post, info, toast, showError, setBusy, runJob, jobBox, wirePickers, adminLink, fmtBytes, PADDLE_CPU_NOTE } = window.App;
  const $ = (id) => document.getElementById(id);
  const SECTIONS = ['server', 'https', 'remote', 'library', 'processor', 'ocr', 'startup', 'logs', 'doctor', 'update', 'advanced'];
  const loaded = {};
  let cfg = null; // GET /app/api/server/config
  let followTimer = null;

  function current() {
    const m = location.pathname.match(/^\/app\/settings\/([a-z-]+)/);
    return m && SECTIONS.includes(m[1]) ? m[1] : null;
  }

  function show(section, push) {
    if (!SECTIONS.includes(section)) section = 'server';
    document.querySelectorAll('main > section[data-section]').forEach((s) => { s.hidden = s.dataset.section !== section; });
    document.querySelectorAll('.settings-rail a').forEach((a) => {
      const on = a.dataset.section === section;
      a.classList.toggle('active', on);
      if (on) a.setAttribute('aria-current', 'page'); else a.removeAttribute('aria-current');
    });
    if (push) history.pushState({ section: section }, '', '/app/settings/' + section);
    if (section !== 'logs' && followTimer) { clearInterval(followTimer); followTimer = null; $('log-follow').checked = false; }
    if (!loaded[section]) {
      loaded[section] = true;
      (LOADERS[section] || (() => {}))();
    }
  }

  // A job button: [data-job=kind] runs into the section's job box.
  function jobRequest(kind) {
    switch (kind) {
      case 'ssl-enable-auto': return { kind: 'ssl-enable', auto_cert: true };
      case 'ssl-enable-files': return { kind: 'ssl-enable', cert: $('ssl-cert').value, key: $('ssl-key').value };
      case 'ssl-generate': return { kind: 'ssl-generate', hostname: $('gen-host').value, days: parseInt($('gen-days').value, 10) || 365 };
      case 'models-list': case 'models-verify': case 'install-ocr-list':
        return { kind: kind, processor: $('ocr-role').value === 'processor' };
      case 'models-download':
        return { kind: kind, processor: $('ocr-role').value === 'processor', engine: $('ocr-engine').value || null };
      case 'doctor': return { kind: kind, processor: $('doc-role').value === 'processor' };
      default: return { kind: kind };
    }
  }

  function wireJobs() {
    document.querySelectorAll('[data-job]').forEach((b) => {
      b.addEventListener('click', async () => {
        const section = b.closest('section[data-section]').dataset.section;
        const holder = $('job-' + section);
        const box = jobBox(holder);
        setBusy(b, true);
        try {
          const done = await runJob(jobRequest(b.dataset.job), box);
          if (b.dataset.job.startsWith('ssl-')) loadHttps();
          if (done.state === 'ok' && b.dataset.job === 'update-apply') toast('Updated. Restart the server and processor to use it.');
        } catch (e) {
          holder.innerHTML = '<div class="alert alert--error">' + esc(e.message) + '</div>';
        } finally { setBusy(b, false); }
      });
    });
  }

  // ---- server -----------------------------------------------------------
  function byKey(obj, key) {
    return key.split('.').reduce((o, k) => (o == null ? undefined : o[k]), obj);
  }

  // The file's value of a field's key; keys left at their default are not written.
  function fileValue(el) {
    const v = byKey(cfg.file, el.dataset.key);
    return v === undefined && el.dataset.default !== undefined ? el.dataset.default : v;
  }

  // MOKURO_* variables that override config.yaml keys (MOKURO_<SECTION>_<KEY> and the
  // short aliases), not the other MOKURO_* knobs.
  function overriding(env, file) {
    const sections = Object.keys(file || {}).map((k) => k.toUpperCase());
    return env.filter((k) => ['MOKURO_HOST', 'MOKURO_PORT', 'MOKURO_STORAGE'].includes(k) ||
      sections.some((sec) => k.startsWith('MOKURO_' + sec + '_')));
  }

  async function loadConfig() {
    cfg = await get('/app/api/server/config');
    return cfg;
  }

  async function loadServer() {
    try {
      const c = await loadConfig();
      $('cfg-path').textContent = c.path;
      $('no-config').hidden = c.exists;
      const overridden = overriding(c.env, c.file);
      $('env-note').hidden = !overridden.length;
      if (overridden.length) {
        $('env-note').textContent = 'Set in the environment, which wins over the file: ' + overridden.join(', ') + '.';
      }
      document.querySelectorAll('#sec-server [data-key]').forEach((el) => {
        const v = fileValue(el);
        if (el.type === 'checkbox') el.checked = !!v;
        else if (Array.isArray(v)) el.value = v.join(', ');
        else el.value = v == null ? '' : v;
      });
      if (!c.ocr_build) {
        ['k-local', 'k-backend', 'k-conc'].forEach((id) => { $(id).disabled = true; });
      }
      refreshHealth();
    } catch (e) { showError($('err-server'), e); }
  }

  async function refreshHealth() {
    try {
      const h = await get('/app/api/server/health');
      $('server-up').innerHTML = h.url
        ? (h.up ? 'Running at <a href="' + esc(h.url) + '" target="_blank" rel="noopener">' + esc(h.url) + '</a>' : 'Not running (' + esc(h.url) + ')')
        : '';
      $('server-start').hidden = !!h.up || !(cfg && cfg.exists);
    } catch (_) { /* ignore */ }
  }

  async function saveServer() {
    const btn = $('server-save');
    showError($('err-server'), null);
    const set = {};
    document.querySelectorAll('#sec-server [data-key]').forEach((el) => {
      if (el.disabled) return;
      const key = el.dataset.key;
      const before = fileValue(el);
      const now = el.type === 'checkbox' ? el.checked : el.value.trim();
      const was = Array.isArray(before) ? before.join(', ') : (el.type === 'checkbox' ? !!before : String(before == null ? '' : before));
      if (String(now) !== String(was)) set[key] = now;
    });
    if (!Object.keys(set).length) { toast('Nothing changed.', 'info'); return; }
    setBusy(btn, true, 'Saving…');
    try {
      const r = await post('/app/api/server/config', { set: set });
      toast('Saved ' + r.changed.join(', ') + '. Restart the server to apply.');
      await loadServer();
    } catch (e) { showError($('err-server'), e); } finally { setBusy(btn, false); }
  }

  async function startServer() {
    const btn = $('server-start');
    setBusy(btn, true, 'Starting…');
    try {
      const r = await post('/app/api/server/start');
      if (r.up) toast('The server is running at ' + r.url);
      else showError($('err-server'), new Error('The server did not answer. Its output:\n' + (r.output || '(nothing)')));
      refreshHealth();
    } catch (e) { showError($('err-server'), e); } finally { setBusy(btn, false); }
  }

  // ---- https ------------------------------------------------------------
  async function loadHttps() {
    try {
      const s = await get('/app/api/ssl');
      $('ssl-kv').innerHTML =
        '<dt>HTTPS</dt><dd>' + (s.enabled ? '<span class="badge badge--success">on</span>' : '<span class="badge badge--muted">off</span>') +
        (s.enabled && s.auto_cert ? ' (self-signed)' : '') + '</dd>' +
        '<dt>Certificate</dt><dd>' + esc(s.cert_file) + (s.cert_exists ? '' : ' (not there)') + '</dd>' +
        '<dt>Key</dt><dd>' + esc(s.key_file) + '</dd>' +
        (s.details || []).map((d) => '<dt></dt><dd>' + esc(d) + '</dd>').join('');
      if (!s.auto_cert && s.enabled) { $('ssl-cert').value = s.cert_file; $('ssl-key').value = s.key_file; }
    } catch (e) { $('ssl-kv').innerHTML = '<dt>Error</dt><dd>' + esc(e.message) + '</dd>'; }
  }

  // ---- remote -----------------------------------------------------------
  async function loadRemote() {
    try {
      const c = cfg || await loadConfig();
      const list = (c.file.cors && c.file.cors.allowed_origins) || [];
      $('cors-list').innerHTML = list.length
        ? list.map((o) => '<li>' + esc(o) + ' <button type="button" class="btn btn--ghost btn--sm" data-cors-remove="' + esc(o) + '">Remove</button></li>').join('')
        : '<li class="text-muted">None.</li>';
    } catch (e) { $('cors-list').innerHTML = '<li class="form-error">' + esc(e.message) + '</li>'; }
  }

  async function cors(body) {
    try {
      await post('/app/api/server/config', body);
      toast('Saved. Restart the server to apply.');
      await loadConfig();
      loadRemote();
    } catch (e) { toast(e.message, 'error'); }
  }

  // ---- admin links ------------------------------------------------------
  async function wireAdmin() {
    const base = await adminLink('').catch(() => null);
    document.querySelectorAll('[data-admin]').forEach((a) => {
      if (base) a.href = base + '#' + a.dataset.admin;
      else { a.removeAttribute('href'); a.classList.add('text-muted'); }
    });
    if (!base) $('admin-down').hidden = false;
  }

  // ---- processor --------------------------------------------------------
  function procForm() {
    const n = (id) => { const v = $(id).value.trim(); return v === '' ? null : Number(v); };
    return {
      url: $('p-url').value.trim(), username: $('p-user').value.trim(), password: $('p-pass').value,
      tls_verify: $('p-tls').value.trim() || 'true', name: $('p-name').value.trim(),
      public_name: $('p-public').value.trim(), max_sessions: n('p-sessions'),
      storage: $('p-storage').value.trim(), archive_memory_mb: n('p-mem'),
      auto_update: $('p-auto').checked,
    };
  }

  function statusLine(s) {
    if (!s || !s.state) return 'Not run yet on this machine.';
    const when = s.updated_at ? new Date(s.updated_at * 1000).toLocaleString() : '';
    return 'Last status: ' + s.state + (s.library ? ' to ' + s.library : '') + (s.sessions ? ', ' + s.sessions + ' session(s)' : '') + (when ? ' (' + when + ')' : '');
  }

  async function loadProcessor() {
    const i = await info();
    $('pcfg-path').textContent = i.processor_config;
    try {
      const p = await get('/app/api/processor/config');
      if (!p.exists) {
        $('no-pcfg').hidden = false;
        $('pform').hidden = true;
        return;
      }
      $('p-url').value = p.url; $('p-user').value = p.username; $('p-tls').value = p.tls_verify;
      $('p-name').value = p.name; $('p-public').value = p.public_name || '';
      $('p-sessions').value = p.max_sessions; $('p-mem').value = p.archive_memory_mb;
      $('p-storage').value = p.storage;
      $('p-auto').checked = !!p.auto_update;
      $('p-pass').placeholder = p.password_set ? '(saved)' : '';
      $('pstatus').textContent = statusLine(p.status);
    } catch (e) { showError($('err-proc'), e); }
  }

  function connectionResult(c, warning) {
    return '<div class="result result--ok"><strong>Connected</strong> to ' + esc(c.url) + ' as ' + esc(c.username) +
      ' (' + esc(c.role) + ')' + (c.library_version ? ', library ' + esc(c.library_version) : '') +
      '<ul class="notes">' + (c.notes || []).concat(warning ? [warning] : []).map((n) => '<li>' + esc(n) + '</li>').join('') + '</ul></div>';
  }

  async function testProcessor() {
    const btn = $('p-test');
    showError($('err-proc'), null);
    const f = procForm();
    if (!f.password) { showError($('err-proc'), new Error('Type the password to test the connection.')); return; }
    setBusy(btn, true, 'Testing…');
    try {
      const r = await post('/app/api/processor/test', f);
      $('ptest').innerHTML = connectionResult(r.connection || r);
    } catch (e) { showError($('err-proc'), e); } finally { setBusy(btn, false); }
  }

  async function saveProcessor() {
    const btn = $('p-save');
    showError($('err-proc'), null);
    setBusy(btn, true, 'Saving…');
    try {
      const r = await post('/app/api/processor/config', procForm());
      $('ptest').innerHTML = r.connection ? connectionResult(r.connection, r.warning) : '';
      $('p-pass').value = '';
      toast('Saved. Restart the processor to apply.');
    } catch (e) { showError($('err-proc'), e); } finally { setBusy(btn, false); }
  }

  async function startProcessor() {
    const btn = $('p-start');
    setBusy(btn, true, 'Starting…');
    showError($('err-proc'), null);
    try {
      const r = await post('/app/api/processor/start');
      $('pstatus').textContent = statusLine(r.status);
      if (r.state === 'connected') toast('The processor is connected.');
      else showError($('err-proc'), new Error('The processor is ' + r.state + '. Its output:\n' + (r.output || '(nothing)')));
    } catch (e) { showError($('err-proc'), e); } finally { setBusy(btn, false); }
  }

  // ---- ocr --------------------------------------------------------------
  // Downloads for paddle-manga: what it costs where it would run on the CPU.
  function renderEngineNote() {
    const paddle = $('ocr-engine').value === 'paddle-manga';
    $('ocr-engine-note').hidden = !paddle;
    $('ocr-engine-note').textContent = paddle ? PADDLE_CPU_NOTE : '';
  }

  async function loadOcr() {
    $('ocr-engine').addEventListener('change', renderEngineNote);
    renderEngineNote();
    const i = await info();
    if (i.role === 'processor' || (!i.config_exists && i.processor_config_exists)) $('ocr-role').value = 'processor';
    try {
      const h = await get('/app/api/ocr/hardware');
      const gpus = (h.nvidia_gpus || []).concat(h.amd_gfx || []);
      $('ocr-kv').innerHTML =
        '<dt>This machine</dt><dd>' + esc(h.target) + (gpus.length ? ' · ' + gpus.map(esc).join(', ') : ' · no GPU found') + '</dd>' +
        '<dt>Best pack</dt><dd>' + esc(h.auto_variant) + ' — ' + esc(h.reason) + '</dd>' +
        '<dt>Installed</dt><dd>' + (h.packs.length
          ? h.packs.map((p) => esc(p.variant) + ' for ' + esc(p.role) + ' (' + esc(p.dir) + ')' + (p.complete ? '' : ' <span class="badge badge--warning">incomplete</span>')).join('<br>')
          : 'none — <a href="/app/setup/ocr">install one</a>') + '</dd>';
    } catch (e) { $('ocr-kv').innerHTML = '<dt>OCR</dt><dd>' + esc(e.message) + '</dd>'; }
  }

  // ---- startup ----------------------------------------------------------
  async function loadStartup() {
    const rows = [];
    for (const role of ['server', 'processor']) {
      try {
        const s = await get('/app/api/service?role=' + role);
        rows.push('<dt>' + (role === 'server' ? 'Library server' : 'Processor') + '</dt><dd>' +
          (s.written ? '<span class="badge badge--success">set up</span> ' + esc(s.enabled || '') : '<span class="badge badge--muted">not set up</span>') +
          ' · ' + esc(s.describe) + '<br><code>' + esc(s.path) + '</code></dd>');
      } catch (e) { rows.push('<dt>' + role + '</dt><dd>' + esc(e.message) + '</dd>'); }
    }
    try {
      const t = await get('/app/api/tray');
      const managed = t.managed.map((m) => m.role);
      rows.push('<dt>Tray</dt><dd>' + (t.available ? esc(t.tray_exe) : 'not in this package') +
        (t.running ? ' <span class="badge badge--success">running</span>' : '') +
        '<br>runs: ' + esc(managed.length ? managed.join(', ') : 'nothing (it only shows what runs)') +
        ' · <code>' + esc(t.config_path) + '</code>' +
        '<br>login item: ' + (t.autostart ? '<span class="badge badge--success">yes</span>' : '<span class="badge badge--muted">no</span>') +
        ' <code>' + esc(t.autostart_path || '') + '</code></dd>');
    } catch (e) { rows.push('<dt>Tray</dt><dd>' + esc(e.message) + '</dd>'); }
    $('startup-kv').innerHTML = rows.join('');
  }

  // ---- logs -------------------------------------------------------------
  async function loadLogs() {
    try {
      const l = await get('/app/api/logs');
      $('logs-dirs').textContent = 'From ' + l.dirs.map((d) => d.dir).join(' and ') + '.';
      const sel = $('log-file');
      sel.innerHTML = l.files.length
        ? l.files.map((f) => '<option value="' + esc(f.id) + '">' + esc(f.role + ' · ' + f.name) + ' (' + fmtBytes(f.size) + ')</option>').join('')
        : '<option value="">No logs yet</option>';
      if (l.files.length) tail();
    } catch (e) { $('log-text').textContent = e.message; }
  }

  async function tail() {
    const id = $('log-file').value;
    if (!id) return;
    const pre = $('log-text');
    try {
      const t = await get('/app/api/logs/tail?lines=400&id=' + encodeURIComponent(id));
      const atEnd = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 4;
      pre.textContent = t.text || '(empty)';
      if (atEnd || !pre.dataset.seen) pre.scrollTop = pre.scrollHeight;
      pre.dataset.seen = '1';
    } catch (e) { pre.textContent = e.message; }
  }

  // ---- update -----------------------------------------------------------
  async function loadUpdate() {
    $('update-kv').innerHTML = '<dt>Status</dt><dd>Checking…</dd>';
    try {
      const u = await get('/app/api/update');
      $('update-kv').innerHTML =
        '<dt>This version</dt><dd>' + esc(u.current) + '</dd>' +
        '<dt>Latest</dt><dd>' + esc(u.latest || '—') + (u.available ? ' <span class="badge badge--info">update available</span>' : '') +
        (u.notes_url ? ' · <a href="' + esc(u.notes_url) + '" target="_blank" rel="noopener">release notes</a>' : '') + '</dd>' +
        '<dt>Installed as</dt><dd>' + esc(u.install) + '</dd>' +
        '<dt>Channel</dt><dd>' + esc(u.channel) + (u.check ? '' : ' (automatic checks off)') + '</dd>' +
        (u.error ? '<dt>Problem</dt><dd>' + esc(u.error) + '</dd>' : '') +
        (u.note ? '<dt>Note</dt><dd>' + esc(u.note) + '</dd>' : '') +
        (u.docker_image ? '<dt>Docker</dt><dd><code>docker pull ' + esc(u.docker_image) + '</code></dd>' : '');
      $('update-apply').disabled = !(u.available && u.can_apply);
    } catch (e) { $('update-kv').innerHTML = '<dt>Problem</dt><dd>' + esc(e.message) + '</dd>'; }
    await loadAutoUpdate();
  }

  // The opt-in: the library server's update.auto and/or the processor's
  // processor.auto_update, whichever this machine has.
  async function loadAutoUpdate() {
    showError($('err-update-auto'), null);
    try {
      const i = await info(true);
      $('update-auto-server-row').hidden = !i.config_exists;
      $('update-auto-proc-row').hidden = !i.processor_config_exists;
      if (i.config_exists) {
        const c = await get('/app/api/server/config');
        $('update-auto-server').checked = byKey(c.file, 'update.auto') === true || (byKey(c.file, 'update.auto') === undefined && byKey(c.effective, 'update.auto') === true);
      }
      if (i.processor_config_exists) {
        const p = await get('/app/api/processor/config');
        $('update-auto-proc').checked = !!p.auto_update;
      }
    } catch (e) { showError($('err-update-auto'), e); }
  }

  async function saveAutoServer() {
    const box = $('update-auto-server');
    showError($('err-update-auto'), null);
    try {
      await post('/app/api/server/config', { set: { 'update.auto': box.checked } });
      loaded.server = false; loaded.advanced = false;
      toast(box.checked ? 'Automatic updates on. Restart the server to apply.' : 'Automatic updates off. Restart the server to apply.');
    } catch (e) { box.checked = !box.checked; showError($('err-update-auto'), e); }
  }

  async function saveAutoProcessor() {
    const box = $('update-auto-proc');
    showError($('err-update-auto'), null);
    try {
      // The processor settings are saved as a whole; the saved ones are sent back as they are.
      const p = await get('/app/api/processor/config');
      await post('/app/api/processor/config', {
        url: p.url, username: p.username, password: '', tls_verify: p.tls_verify, name: p.name,
        public_name: p.public_name || '', max_sessions: p.max_sessions, storage: p.storage,
        archive_memory_mb: p.archive_memory_mb, auto_update: box.checked,
      });
      loaded.processor = false;
      toast(box.checked ? 'Automatic updates on. Restart the processor to apply.' : 'Automatic updates off. Restart the processor to apply.');
    } catch (e) { box.checked = !box.checked; showError($('err-update-auto'), e); }
  }

  // ---- advanced ---------------------------------------------------------
  async function loadAdvanced() {
    try {
      const c = await loadConfig();
      const sel = $('adv-key');
      const keep = sel.value;
      sel.innerHTML = c.keys.map((k) => '<option>' + esc(k) + '</option>').join('');
      if (keep) sel.value = keep;
      fillAdvValue();
      $('yaml').textContent = c.yaml + (c.warnings.length ? '\n# warnings:\n' + c.warnings.map((w) => '#  ' + w).join('\n') : '');
    } catch (e) { showError($('err-adv'), e); }
  }

  function fillAdvValue() {
    if (!cfg) return;
    const key = $('adv-key').value;
    let v = byKey(cfg.file, key);
    if (v === undefined) v = byKey(cfg.effective, key);
    $('adv-value').value = Array.isArray(v) ? v.join(', ') : (v == null ? '' : (typeof v === 'object' ? JSON.stringify(v) : v));
  }

  async function setAny() {
    const btn = $('adv-set');
    showError($('err-adv'), null);
    setBusy(btn, true);
    try {
      const set = {}; set[$('adv-key').value] = $('adv-value').value;
      await post('/app/api/server/config', { set: set });
      toast('Saved. Restart the server to apply.');
      loaded.server = false;
      await loadAdvanced();
    } catch (e) { showError($('err-adv'), e); } finally { setBusy(btn, false); }
  }

  async function initConfig() {
    if (!confirm('Replace config.yaml with the defaults? Users, the library and its folder are not touched.')) return;
    try {
      await post('/app/api/server/config', { init: true, force: true });
      toast('config.yaml reset to the defaults.');
      loaded.server = false;
      loadAdvanced();
    } catch (e) { showError($('err-adv'), e); }
  }

  const LOADERS = {
    server: loadServer, https: loadHttps, remote: loadRemote, library: () => {}, processor: loadProcessor,
    ocr: loadOcr, startup: loadStartup, logs: loadLogs, doctor: () => {}, update: loadUpdate, advanced: loadAdvanced,
  };

  document.addEventListener('DOMContentLoaded', async () => {
    wirePickers();
    wireJobs();
    wireAdmin();
    document.querySelectorAll('.settings-rail a[data-section]').forEach((a) => {
      a.addEventListener('click', (ev) => {
        if (ev.ctrlKey || ev.metaKey || ev.shiftKey) return;
        ev.preventDefault();
        show(a.dataset.section, true);
      });
    });
    window.addEventListener('popstate', () => show(current() || 'server', false));
    $('server-save').onclick = saveServer;
    $('server-start').onclick = startServer;
    $('cors-add').onclick = () => { const o = $('cors-new').value.trim(); if (o) { $('cors-new').value = ''; cors({ cors_add: o }); } };
    $('cors-list').addEventListener('click', (ev) => {
      const b = ev.target.closest('[data-cors-remove]');
      if (b) cors({ cors_remove: b.dataset.corsRemove });
    });
    $('p-test').onclick = testProcessor;
    $('p-save').onclick = saveProcessor;
    $('p-start').onclick = startProcessor;
    $('log-file').onchange = () => { delete $('log-text').dataset.seen; tail(); };
    $('log-refresh').onclick = loadLogs;
    $('log-follow').onchange = () => {
      if (followTimer) { clearInterval(followTimer); followTimer = null; }
      if ($('log-follow').checked) followTimer = setInterval(tail, 3000);
    };
    $('update-check').onclick = loadUpdate;
    $('update-auto-server').onchange = saveAutoServer;
    $('update-auto-proc').onchange = saveAutoProcessor;
    $('adv-key').onchange = fillAdvValue;
    $('adv-set').onclick = setAny;
    $('cfg-init').onclick = initConfig;

    let first = current();
    try {
      const i = await info();
      if (!i.ocr_build) document.querySelectorAll('[data-ocr]').forEach((el) => { el.hidden = true; });
      if (!first) first = i.role === 'processor' || (!i.config_exists && i.processor_config_exists) ? 'processor' : 'server';
      if (!i.ocr_build && (first === 'processor' || first === 'ocr')) first = 'server';
    } catch (e) { showError($('page-error'), e); }
    show(first || 'server', false);
  });
})();
