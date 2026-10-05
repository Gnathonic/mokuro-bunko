// Library server wizard: the `setup` questions, then save (config.yaml + admin
// account + certificate) and start `serve` in the background.
(function () {
  'use strict';
  const { esc, get, post, info, showError, setBusy, wirePickers, wizard } = window.App;
  const $ = (id) => document.getElementById(id);
  const val = (name) => (document.querySelector('input[name="' + name + '"]:checked') || {}).value;
  let w;
  let started = null;

  function form() {
    const admin = $('make-admin').checked
      ? { username: $('admin-user').value.trim(), password: $('admin-pass').value } : null;
    return {
      storage: $('storage').value.trim(),
      host: val('host'),
      port: parseInt($('port').value, 10) || 0,
      admin: admin,
      registration_mode: val('reg'),
      access: val('access'),
      dyndns: { provider: $('dd-provider').value, domain: $('dd-domain').value.trim(),
        token: $('dd-token').value.trim(), update_url: $('dd-url').value.trim() },
      ssl: { mode: val('ssl'), cert_file: $('cert-file').value.trim(), key_file: $('key-file').value.trim() },
      cors_origins: $('cors').value.split('\n').map((s) => s.trim()).filter(Boolean),
      local_ocr: $('local-ocr').checked,
      overwrite: $('overwrite').checked,
    };
  }

  function check(step) {
    const f = form();
    if (step === 'folder') {
      if (!f.storage) return 'Choose the library folder.';
      if (!(f.port >= 1 && f.port <= 65535)) return 'The port is a number from 1 to 65535.';
    }
    if (step === 'admin' && f.admin) {
      if (!/^[A-Za-z0-9_-]{3,32}$/.test(f.admin.username)) return 'The username is 3–32 letters, digits, _ or -.';
      if (f.admin.password.length < 8) return 'The password needs at least 8 characters.';
      if (f.admin.password !== $('admin-pass2').value) return 'The two passwords differ.';
    }
    if (step === 'remote') {
      if (f.access === 'dyndns' && (!f.dyndns.domain || !f.dyndns.token)) return 'Dynamic DNS needs the domain and the token.';
      if (f.access === 'dyndns' && f.dyndns.provider === 'generic' && !f.dyndns.update_url) return 'The generic provider needs an update URL.';
      if (f.ssl.mode === 'files' && (!f.ssl.cert_file || !f.ssl.key_file)) return 'Choose the certificate and the key files.';
    }
    return null;
  }

  function summary() {
    const f = form();
    const rows = [
      ['Config file', state.info.config_path],
      ['Library folder', f.storage],
      ['Address', (f.host === '0.0.0.0' ? 'everyone on the network' : 'this computer only') + ', port ' + f.port],
      ['Admin', f.admin ? f.admin.username : '(none now)'],
      ['Registration', f.registration_mode],
      ['Remote access', f.access + (f.access === 'dyndns' ? ' (' + f.dyndns.domain + ')' : '')],
      ['HTTPS', f.ssl.mode],
      ['OCR here', state.info.ocr_build ? (f.local_ocr ? 'yes' : 'no, processors only') : 'no (lite build)'],
    ];
    if (f.cors_origins.length) rows.push(['CORS', f.cors_origins.join(', ')]);
    $('summary').innerHTML = rows.map(([k, v]) => '<dt>' + esc(k) + '</dt><dd>' + esc(v) + '</dd>').join('');
  }

  const state = { info: null };

  async function save() {
    const btn = $('save-btn');
    const err = $('err-review');
    showError(err, null);
    setBusy(btn, true, 'Saving…');
    const out = $('save-result');
    try {
      const r = await post('/app/api/server/setup', form());
      out.innerHTML = '<div class="result result--ok"><strong>Saved.</strong><ul class="notes">' +
        r.notes.map((n) => '<li>' + esc(n) + '</li>').join('') + '</ul></div>' +
        '<div class="result result--wait" id="start-wait">Starting the server…</div>';
      $('review-back').disabled = true;
      setBusy(btn, true, 'Starting…');
      const s = await post('/app/api/server/start');
      started = s;
      if (!s.up) {
        $('start-wait').outerHTML = '<div class="result result--bad"><strong>The server did not answer at ' + esc(s.url) +
          '.</strong><pre class="log-view">' + esc(s.output || '') + '</pre><p class="form-hint">Its output: ' + esc(s.log || '') + '</p></div>';
        setBusy(btn, false);
        btn.textContent = 'Try starting again';
        btn.onclick = retryStart;
        return;
      }
      done(s);
    } catch (e) {
      showError(err, e);
      setBusy(btn, false);
      if (e.message && e.message.includes('already exists')) $('overwrite-row').hidden = false;
    }
  }

  async function retryStart() {
    const btn = $('save-btn');
    setBusy(btn, true, 'Starting…');
    try {
      const s = await post('/app/api/server/start');
      if (s.up) done(s); else { setBusy(btn, false); App.toast('Still not answering; see the output above.', 'error'); }
    } catch (e) { showError($('err-review'), e); setBusy(btn, false); }
  }

  async function done(s) {
    const i = await info(true);
    const admin = i.library_url + (i.admin_path || '/_admin');
    $('done-body').innerHTML =
      '<div class="result result--ok">' + (s.already_running ? 'A server was already running at ' : 'Answering at ') +
      '<a href="' + esc(s.url) + '" target="_blank" rel="noopener">' + esc(s.url) + '</a>.</div>' +
      '<p class="form-hint mt-2">It runs in the background' + (s.pid ? ' (pid ' + esc(s.pid) + ')' : '') +
      '. To start it at every login, use the next step.</p>';
    const links = [
      '<li><a href="' + esc(s.url) + '" target="_blank" rel="noopener">Open the library</a></li>',
      '<li><a href="' + esc(admin) + '" target="_blank" rel="noopener">Open the admin panel</a> (users, invites, settings, tunnel)</li>',
    ];
    if (i.ocr_build && $('local-ocr').checked) links.push('<li><a href="/app/setup/ocr?role=server">Install OCR on this machine</a></li>');
    links.push('<li><a href="/app/setup/startup?role=server">Start the server with the machine</a></li>');
    links.push('<li><a href="#" id="go-dashboard">Go to its dashboard</a> <span class="form-hint" id="dash-note"></span></li>');
    $('done-links').innerHTML = links.join('');
    $('go-dashboard').addEventListener('click', goDashboard);
    w.go('s-done');
  }

  // The started server serves its own pages (its control listener): move there and
  // let this setup app close.
  async function goDashboard(ev) {
    ev.preventDefault();
    const note = $('dash-note');
    note.textContent = 'Looking for it…';
    for (let k = 0; k < 20; k++) {
      try {
        const r = await get('/app/api/instances');
        const me = r.instances.find((x) => x.role === 'server' && x.alive && x.dashboard);
        if (me) {
          await post('/app/api/handoff', { url: me.dashboard }).catch(() => {});
          window.location.href = me.dashboard;
          return;
        }
      } catch (_) { /* retry */ }
      await new Promise((r) => setTimeout(r, 500));
    }
    note.textContent = 'Its dashboard is not reachable (control API off?). Open the library instead.';
  }

  document.addEventListener('DOMContentLoaded', async () => {
    wirePickers();
    w = wizard(['s-folder', 's-admin', 's-registration', 's-remote', 's-ocr', 's-review', 's-done'], (id) => {
      if (id === 's-review') summary();
    });
    document.querySelectorAll('[data-next]').forEach((b) => b.addEventListener('click', () => {
      const step = b.dataset.next;
      const msg = check(step);
      const err = $('err-' + step);
      if (err) showError(err, msg ? new Error(msg) : null);
      if (!msg) w.next();
    }));
    document.querySelectorAll('[data-prev]').forEach((b) => b.addEventListener('click', () => w.prev()));
    $('make-admin').addEventListener('change', () => { $('admin-fields').hidden = !$('make-admin').checked; });
    document.querySelectorAll('input[name="access"]').forEach((r) => r.addEventListener('change', () => {
      $('dyndns-fields').hidden = val('access') !== 'dyndns';
    }));
    $('dd-provider').addEventListener('change', () => { $('dd-url-group').hidden = $('dd-provider').value !== 'generic'; });
    document.querySelectorAll('input[name="ssl"]').forEach((r) => r.addEventListener('change', () => {
      $('ssl-files').hidden = val('ssl') !== 'files';
    }));
    $('save-btn').addEventListener('click', save);
    $('server-form').addEventListener('submit', (e) => e.preventDefault());

    try {
      const i = await info();
      state.info = i;
      $('storage').value = i.server_storage || '';
      if (i.config_exists) {
        const c = await get('/app/api/server/config');
        const f = c.file;
        $('storage').value = f.storage.base_path;
        $('port').value = f.server.port;
        const host = document.querySelector('input[name="host"][value="' + f.server.host + '"]');
        if (host) host.checked = true;
        const reg = document.querySelector('input[name="reg"][value="' + f.registration.mode + '"]');
        if (reg) reg.checked = true;
        $('local-ocr').checked = !!f.ocr.local_processing;
        $('exists-note').hidden = false;
        $('exists-note').textContent = 'This machine already has a library configuration (' + i.config_path +
          '). The answers below start from it; saving replaces it. To change single settings, use Settings instead.';
        $('overwrite-row').hidden = false;
        $('make-admin').checked = false;
        $('admin-fields').hidden = true;
      }
      if (!i.ocr_build) {
        $('local-ocr').checked = false;
        $('local-ocr').disabled = true;
        $('ocr-note').textContent = 'This is the lite build: OCR runs on processors (the full build on another machine).';
      } else {
        $('ocr-note').textContent = 'Needs the OCR backend for this machine: the OCR step installs it after the server starts.';
      }
    } catch (e) {
      showError($('err-folder'), e);
    }
  });
})();
