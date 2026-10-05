// Processor wizard: library + account (tested), this machine's settings, save
// processor.yaml, start `processor serve` and wait for it to connect.
(function () {
  'use strict';
  const { esc, get, post, info, showError, setBusy, wirePickers, wizard } = window.App;
  const $ = (id) => document.getElementById(id);
  const val = (name) => (document.querySelector('input[name="' + name + '"]:checked') || {}).value;
  let w;
  let tested = null;

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
    ];
    $('p-summary').innerHTML = rows.map(([k, v]) => '<dt>' + esc(k) + '</dt><dd>' + esc(v) + '</dd>').join('');
  }

  async function save() {
    const btn = $('p-save-btn');
    showError($('err-save'), null);
    setBusy(btn, true, 'Checking and saving…');
    try {
      const r = await post('/app/api/processor/setup', form());
      $('p-save-result').innerHTML = '<div class="result result--ok"><strong>Saved</strong> ' + esc(r.config) +
        (r.warning ? '<p>' + esc(r.warning) + '</p>' : ' (readable by you only).') + '</div>';
      btn.hidden = true;
      $('p-save-back').disabled = true;
      $('p-start-btn').hidden = false;
      $('p-start-btn').focus();
    } catch (e) {
      showError($('err-save'), e);
      if (e.message && e.message.includes('already exists')) $('p-overwrite-row').hidden = false;
      setBusy(btn, false);
    }
  }

  async function start() {
    const btn = $('p-start-btn');
    setBusy(btn, true, 'Starting and connecting…');
    try {
      const r = await post('/app/api/processor/start');
      if (r.state !== 'connected') {
        $('p-save-result').innerHTML += '<div class="result result--bad"><strong>The processor says: ' + esc(r.state) +
          (r.status && r.status.error ? ' — ' + esc(r.status.error) : '') + '</strong>' +
          (r.output ? '<pre class="log-view">' + esc(r.output) + '</pre>' : '') +
          '<p class="form-hint">' + (r.running ? 'It keeps trying in the background.' : 'It stopped.') + ' Output: ' + esc(r.log) + '</p></div>';
        setBusy(btn, false);
        btn.textContent = 'Start again';
        if (!r.running) return;
      }
      done(r);
    } catch (e) {
      showError($('err-save'), e);
      setBusy(btn, false);
    }
  }

  function done(r) {
    const s = r.status || {};
    $('p-done-body').innerHTML = '<div class="result ' + (r.state === 'connected' ? 'result--ok' : 'result--wait') + '">' +
      (r.state === 'connected' ? 'Connected to ' : 'Started; state ' + esc(r.state) + ' for ') +
      esc(s.library || form().url) + (s.name ? ' as ' + esc(s.name) : '') + ' (pid ' + esc(r.pid) + ').</div>' +
      '<p class="form-hint mt-2">It runs in the background. The library\'s admin panel lists it under Settings → OCR → Processors.</p>';
    $('p-done-links').innerHTML = [
      App._info.ocr_build ? '<li><a href="/app/setup/ocr?role=processor">Install the OCR backend for this processor</a> (the GPU pack and the models)</li>' : '',
      '<li><a href="/app/setup/startup?role=processor">Start the processor with the machine</a></li>',
      '<li><a href="#" id="p-dash">Go to its dashboard</a> <span class="form-hint" id="p-dash-note"></span></li>',
    ].join('');
    $('p-dash').addEventListener('click', async (ev) => {
      ev.preventDefault();
      const note = $('p-dash-note');
      note.textContent = 'Looking for it…';
      for (let k = 0; k < 20; k++) {
        try {
          const x = await get('/app/api/instances');
          const me = x.instances.find((i) => i.role === 'processor' && i.alive && i.dashboard);
          if (me) {
            await post('/app/api/handoff', { url: me.dashboard }).catch(() => {});
            window.location.href = me.dashboard;
            return;
          }
        } catch (_) { /* retry */ }
        await new Promise((res) => setTimeout(res, 500));
      }
      note.textContent = 'Its dashboard is not reachable yet.';
    });
    w.go('p-done');
  }

  document.addEventListener('DOMContentLoaded', async () => {
    wirePickers();
    w = wizard(['p-library', 'p-machine', 'p-save', 'p-done'], (id) => { if (id === 'p-save') summary(); });
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
    $('p-save-btn').addEventListener('click', save);
    $('p-start-btn').addEventListener('click', start);
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
      const c = await get('/app/api/processor/config');
      if (c.exists) {
        $('url').value = c.url;
        $('username').value = c.username;
        $('name').value = c.name;
        $('public-name').value = c.public_name || '';
        $('sessions').value = c.max_sessions;
        $('pstorage').value = c.storage;
        $('archive-mb').value = c.archive_memory_mb;
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
        $('archive-mb').value = c.defaults.archive_memory_mb;
      }
    } catch (e) {
      $('test-result').innerHTML = '<div class="result result--bad">' + esc(e.message) + '</div>';
    }
  });
})();
