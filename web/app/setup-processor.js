// Processor wizard: library + account (tested), this machine's settings, the OCR
// install (always: a processor without OCR does nothing) and optionally start with the
// machine. The review step saves processor.yaml, installs OCR, sets up the start-up,
// starts `processor serve` and waits for it to connect.
(function () {
  'use strict';
  const { esc, get, post, info, showError, setBusy, wirePickers, wizard } = window.App;
  const S = window.SetupSteps;
  const $ = (id) => document.getElementById(id);
  const val = (name) => (document.querySelector('input[name="' + name + '"]:checked') || {}).value;
  let w;
  let tested = null;
  const state = { ocr: null, startup: null };
  const finished = {};
  let failedStage = null;
  let doctorOk = true;
  let mark = () => {};
  const startupOn = () => $('want-startup').checked;

  function steps() {
    return ['p-library', 'p-machine', 'p-ocr'].concat(startupOn() ? ['p-startup'] : [], ['p-save', 'p-done']);
  }

  function tlsValue() {
    const v = val('tls');
    return v === 'cert' ? $('tls-cert').value.trim() : v;
  }

  function form() {
    return {
      url: $('url').value.trim(),
      username: $('username').value.trim(),
      password: $('password').value,
      tls_verify: tlsValue(),
      name: $('name').value.trim(),
      public_name: $('public-name').value.trim(),
      max_sessions: parseInt($('sessions').value, 10) || 1,
      storage: $('pstorage').value.trim(),
      archive_memory_mb: parseInt($('archive-mb').value, 10),
      auto_update: $('auto-update').checked,
      overwrite: $('p-overwrite').checked,
    };
  }

  async function test() {
    const btn = $('test-btn');
    const out = $('test-result');
    const f = form();
    if (!f.url || !f.username || !f.password) {
      out.innerHTML = '<div class="result result--bad">The address, the username and the password are needed.</div>';
      return null;
    }
    setBusy(btn, true, 'Testing…');
    out.innerHTML = '<div class="result result--wait">Logging in to ' + esc(f.url) + '…</div>';
    try {
      const r = await post('/app/api/processor/test', f);
      tested = r;
      $('url').value = r.url;
      out.innerHTML = '<div class="result result--ok"><strong>Connected.</strong> ' + esc(r.username) +
        ' is a processor account on this library (protocol ' + esc(r.protocol) +
        (r.library_version ? ', mokuro-bunko ' + esc(r.library_version) : '') + ').' +
        (r.notes.length ? '<ul class="notes">' + r.notes.map((n) => '<li>' + esc(n) + '</li>').join('') + '</ul>' : '') + '</div>';
      return r;
    } catch (e) {
      tested = null;
      out.innerHTML = '<div class="result result--bad"><strong>Not connected.</strong><p style="white-space:pre-line">' + esc(e.message) + '</p></div>';
      return null;
    } finally {
      setBusy(btn, false);
    }
  }

  function summary() {
    const f = form();
    const rows = [
      ['Config file', App._info.processor_config],
      ['Library', f.url],
      ['Account', f.username],
      ['Certificate check', f.tls_verify],
      ['Name', f.name || '(this computer\'s name)'],
      ['Public name', f.public_name || '(a number)'],
      ['Volumes at once', f.max_sessions],
      ['Working folder', f.storage],
      ['Archive RAM', f.archive_memory_mb + ' MB'],
      ['Automatic updates', f.auto_update ? 'on' : 'off'],
      ['OCR', state.ocr ? state.ocr.describe() : '-'],
      ['Start with the machine', startupOn() && state.startup ? state.startup.describe() : 'no'],
    ];
    $('p-summary').innerHTML = rows.map(([k, v]) => '<dt>' + esc(k) + '</dt><dd>' + esc(v) + '</dd>').join('');
  }

  function stages() {
    const s = [['save', 'Save processor.yaml'], ['ocr', 'Install OCR'], ['doctor', 'Check (doctor)']];
    if (startupOn()) s.push(['startup', 'Start with the machine']);
    s.push(['start', 'Start the processor']);
    return s;
  }

  async function run() {
    const btn = $('p-save-btn');
    const err = $('err-save');
    showError(err, null);
    $('skip-btn').hidden = true;
    failedStage = null;
    if (!finished.save) {
      $('run-list').hidden = false;
      mark = S.runList($('run-list'), stages());
    }
    setBusy(btn, true, 'Working…');
    $('p-save-back').disabled = true;
    for (const [k] of stages()) {
      if (finished[k]) continue;
      mark(k, 'running');
      let ok = false;
      try {
        ok = await stage(k);
      } catch (e) {
        showError(err, e);
        mark(k, 'failed', e.message && e.message.length < 60 ? e.message : '');
      }
      if (!ok) {
        failedStage = k;
        setBusy(btn, false);
        btn.textContent = 'Try again';
        if (k === 'save') $('p-save-back').disabled = false;
        if (k === 'startup') {
          $('skip-btn').hidden = false;
          $('skip-btn').textContent = 'Skip this';
        }
        return;
      }
      finished[k] = true;
    }
  }

  function skip() {
    if (failedStage !== 'startup') return;
    finished.startup = true;
    mark('startup', 'skipped', 'skipped: Settings can do it later');
    run();
  }

  async function stage(k) {
    if (k === 'save') {
      try {
        const r = await post('/app/api/processor/setup', form());
        $('p-save-result').innerHTML = '<p class="form-hint">Saved ' + esc(r.config) +
          (r.warning ? '. ' + esc(r.warning) : ' (readable by you only).') + '</p>';
        mark('save', 'ok');
        return true;
      } catch (e) {
        if (e.message && e.message.includes('already exists')) $('p-overwrite-row').hidden = false;
        throw e;
      }
    }
    if (k === 'ocr') {
      const r = await S.runOcr(state.ocr.request(), $('install-job'), $('doctor-job'), mark);
      if (r.ok) { finished.doctor = true; doctorOk = r.doctor_ok; }
      return r.ok;
    }
    if (k === 'doctor') return true; // runs with the install
    if (k === 'startup') {
      const m = await state.startup.apply();
      mark('startup', 'ok', m.length ? m[m.length - 1] : '');
      return true;
    }
    if (k === 'start') {
      let r;
      if (startupOn() && state.startup.startsIt()) {
        // The tray or the service starts it: wait for it to connect.
        const st = await S.waitFor(async () => {
          const x = await get('/app/api/processor/status');
          return x.status && x.status.state === 'connected' ? x : null;
        }, 60);
        r = st ? { state: 'connected', status: st.status, running: true } : await post('/app/api/processor/start');
      } else {
        r = await post('/app/api/processor/start');
      }
      if (r.state !== 'connected') {
        mark('start', 'failed', r.state);
        $('p-save-result').innerHTML += '<div class="result result--bad"><strong>The processor says: ' + esc(r.state) +
          (r.status && r.status.error ? ' — ' + esc(r.status.error) : '') + '</strong>' +
          (r.output ? '<pre class="log-view">' + esc(r.output) + '</pre>' : '') +
          '<p class="form-hint">' + (r.running ? 'It keeps trying in the background.' : 'It stopped.') + (r.log ? ' Output: ' + esc(r.log) : '') + '</p></div>';
        if (!r.running) return false;
      } else {
        mark('start', 'ok');
      }
      done(r);
      return true;
    }
    return true;
  }

  function done(r) {
    const s = r.status || {};
    $('p-done-body').innerHTML = '<div class="result ' + (r.state === 'connected' ? 'result--ok' : 'result--wait') + '">' +
      (r.state === 'connected' ? 'Connected to ' : 'Started; state ' + esc(r.state) + ' for ') +
      esc(s.library || form().url) + (s.name ? ' as ' + esc(s.name) : '') + (r.pid ? ' (pid ' + esc(r.pid) + ')' : '') + '.</div>' +
      (doctorOk ? '' : '<p class="form-hint mt-2">The check found problems: see <a href="/app/settings/doctor">Diagnostics</a>.</p>') +
      '<p class="form-hint mt-2">The library\'s admin panel lists it under Settings → OCR → Processors.</p>';
    $('p-done-links').innerHTML = [
      '<li><a href="#" id="p-dash">Go to its dashboard</a> <span class="form-hint" id="p-dash-note"></span></li>',
      '<li class="form-hint">OCR and start-up can change later: <a href="/app/settings/ocr">OCR &amp; models</a>, <a href="/app/settings/startup">Start-up</a>.</li>',
    ].join('');
    $('p-dash').addEventListener('click', async (ev) => {
      ev.preventDefault();
      const note = $('p-dash-note');
      note.textContent = 'Looking for it…';
      const me = await S.waitFor(async () => {
        const x = await get('/app/api/instances');
        return x.instances.find((i) => i.role === 'processor' && i.alive && i.dashboard);
      }, 10);
      if (me) {
        await post('/app/api/handoff', { url: me.dashboard }).catch(() => {});
        window.location.href = me.dashboard;
        return;
      }
      note.textContent = 'Its dashboard is not reachable yet.';
    });
    w.go('p-done');
  }

  document.addEventListener('DOMContentLoaded', async () => {
    wirePickers();
    w = wizard(steps, (id) => { if (id === 'p-save') summary(); }, { done: 'p-done' });
    $('want-startup').addEventListener('change', async () => {
      w.refresh();
      if (startupOn() && !state.startup) {
        try { state.startup = await S.startupStep($('startup-fields'), 'processor'); } catch (e) { showError($('err-startup'), e); }
      }
    });
    $('ocr-next').addEventListener('click', () => {
      const msg = state.ocr ? null : 'The hardware is not detected yet.';
      showError($('err-ocr'), msg ? new Error(msg) : null);
      if (!msg) w.next();
    });
    $('startup-next').addEventListener('click', () => {
      const msg = state.startup ? null : 'The start-up choices are not loaded yet.';
      showError($('err-startup'), msg ? new Error(msg) : null);
      if (!msg) w.next();
    });
    $('skip-btn').addEventListener('click', skip);
    document.querySelectorAll('[data-prev]').forEach((b) => b.addEventListener('click', () => w.prev()));
    document.querySelectorAll('input[name="tls"]').forEach((r) => r.addEventListener('change', () => {
      $('tls-cert-group').hidden = val('tls') !== 'cert';
    }));
    $('test-btn').addEventListener('click', test);
    $('lib-next').addEventListener('click', async () => {
      if (tested || await test()) w.next();
    });
    ['url', 'username', 'password', 'tls-cert'].forEach((id) => $(id).addEventListener('input', () => { tested = null; }));
    $('machine-next').addEventListener('click', () => {
      const f = form();
      const msg = !(f.max_sessions >= 1) ? 'At least one volume at a time.'
        : !(f.archive_memory_mb >= 0) ? 'The archive RAM is a number of MB (0 or more).'
          : !f.storage ? 'Choose the working folder.' : null;
      showError($('err-machine'), msg ? new Error(msg) : null);
      if (!msg) w.next();
    });
    $('p-save-btn').addEventListener('click', run);
    $('proc-form').addEventListener('submit', (e) => e.preventDefault());
    try {
      const i = await info();
      App._info = i;
      if (!i.ocr_build) {
        $('test-result').innerHTML = '<div class="result result--bad">This is the lite build: it has no processor. Install the full build on this machine.</div>';
        $('lib-next').disabled = true;
        $('test-btn').disabled = true;
        return;
      }
      get('/app/api/ocr/hardware').then((hw) => {
        state.ocr = S.ocrStep($('ocr-fields'), 'processor', hw);
      }).catch((e) => showError($('err-ocr'), e));
      const c = await get('/app/api/processor/config');
      if (c.exists) {
        $('url').value = c.url;
        $('username').value = c.username;
        $('name').value = c.name;
        $('public-name').value = c.public_name || '';
        $('sessions').value = c.max_sessions;
        $('pstorage').value = c.storage;
        $('archive-mb').value = c.archive_memory_mb;
        $('auto-update').checked = !!c.auto_update;
        if (c.tls_verify === 'false') document.querySelector('input[name="tls"][value="false"]').checked = true;
        else if (c.tls_verify !== 'true') {
          document.querySelector('input[name="tls"][value="cert"]').checked = true;
          $('tls-cert').value = c.tls_verify;
          $('tls-cert-group').hidden = false;
        }
        $('p-exists').hidden = false;
        $('p-exists').textContent = 'This machine already has ' + c.config + '. Saving replaces it; to change single settings use Settings → Processor.';
        $('p-overwrite-row').hidden = false;
      } else {
        $('name').value = c.defaults.name;
        $('pstorage').value = c.defaults.storage;
        window.App.storageNote($('pstorage-note'), $('pstorage'), i.processor_storage_check);
        $('archive-mb').value = c.defaults.archive_memory_mb;
      }
    } catch (e) {
      $('test-result').innerHTML = '<div class="result result--bad">' + esc(e.message) + '</div>';
    }
  });
})();
