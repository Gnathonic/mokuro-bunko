// Processor settings: one page, the section chosen by the path (/app/settings/<section>).
// A library server's settings live in its admin panel.
(function () {
  'use strict';
  const { esc, get, post, info, toast, showError, setBusy, runJob, jobBox, wirePickers, fmtBytes, PADDLE_CPU_NOTE } = window.App;
  const $ = (id) => document.getElementById(id);
  const SECTIONS = ['processor', 'ocr', 'logs', 'doctor', 'update'];
  const loaded = {};
  let followTimer = null;

  function current() {
    const m = location.pathname.match(/^\/app\/settings\/([a-z-]+)/);
    return m && SECTIONS.includes(m[1]) ? m[1] : null;
  }

  function show(section, push) {
    if (!SECTIONS.includes(section)) section = 'processor';
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

  // A job button: [data-job=kind] runs into the section's job box (for the processor).
  function jobRequest(kind) {
    switch (kind) {
      case 'models-download':
        return { kind: kind, processor: true, engine: $('ocr-engine').value || null };
      case 'models-list': case 'models-verify': case 'install-ocr-list': case 'doctor':
        return { kind: kind, processor: true };
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
          if (done.state === 'ok' && b.dataset.job === 'update-apply') toast('Updated. Quit and start the processor again to use it.');
        } catch (e) {
          holder.innerHTML = '<div class="alert alert--error">' + esc(e.message) + '</div>';
        } finally { setBusy(b, false); }
      });
    });
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
    try {
      const h = await get('/app/api/ocr/hardware');
      const gpus = (h.nvidia_gpus || []).concat(h.amd_gfx || []);
      const packs = h.packs.filter((p) => p.role !== 'server');
      $('ocr-kv').innerHTML =
        '<dt>This machine</dt><dd>' + esc(h.target) + (gpus.length ? ' · ' + gpus.map(esc).join(', ') : ' · no GPU found') + '</dd>' +
        '<dt>Best pack</dt><dd>' + esc(h.auto_variant) + ' — ' + esc(h.reason) + '</dd>' +
        '<dt>Installed</dt><dd>' + (packs.length
          ? packs.map((p) => esc(p.variant) + ' (' + esc(p.dir) + ')' + (p.complete ? '' : ' <span class="badge badge--warning">incomplete</span>')).join('<br>')
          : 'none — <a href="/app/setup/ocr">install one</a>') + '</dd>';
    } catch (e) { $('ocr-kv').innerHTML = '<dt>OCR</dt><dd>' + esc(e.message) + '</dd>'; }
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
        '<dt>Channel</dt><dd>' + esc(u.channel) + (u.channel_setting === 'auto' ? ' (follows this build)' : '') + (u.check ? '' : ' (automatic checks off)') + '</dd>' +
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
      $('update-auto-proc-row').hidden = !i.processor_config_exists;
      if (i.processor_config_exists) {
        const p = await get('/app/api/processor/config');
        $('update-auto-proc').checked = !!p.auto_update;
      }
    } catch (e) { showError($('err-update-auto'), e); }
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

  const LOADERS = { processor: loadProcessor, ocr: loadOcr, logs: loadLogs, doctor: () => {}, update: loadUpdate };

  document.addEventListener('DOMContentLoaded', async () => {
    wirePickers();
    wireJobs();
    document.querySelectorAll('.settings-rail a[data-section]').forEach((a) => {
      a.addEventListener('click', (ev) => {
        if (ev.ctrlKey || ev.metaKey || ev.shiftKey) return;
        ev.preventDefault();
        show(a.dataset.section, true);
      });
    });
    window.addEventListener('popstate', () => show(current() || 'processor', false));
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
    $('update-auto-proc').onchange = saveAutoProcessor;
    show(current() || 'processor', false);
  });
})();
