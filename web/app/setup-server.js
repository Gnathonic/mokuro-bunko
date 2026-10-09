// Library server wizard: the `setup` questions and two toggles (OCR on this machine,
// start with the machine) that add their steps. The review step saves (config.yaml +
// admin account + certificate), installs OCR, sets up the start-up and starts `serve`.
(function () {
  'use strict';
  const { esc, get, post, info, showError, setBusy, wirePickers, wizard } = window.App;
  const S = window.SetupSteps;
  const $ = (id) => document.getElementById(id);
  const val = (name) => (document.querySelector('input[name="' + name + '"]:checked') || {}).value;
  let w;
  const state = { info: null, hw: null, ocr: null, startup: null };
  // Stages of the run that finished (a retry goes on from the first other one).
  const finished = {};
  let failedStage = null;
  let doctorOk = true;

  const ocrOn = () => $('local-ocr').checked;
  const startupOn = () => $('want-startup').checked;

  function steps() {
    return ['s-folder', 's-admin', 's-registration', 's-remote', 's-extras']
      .concat(ocrOn() ? ['s-ocr'] : [], startupOn() ? ['s-startup'] : [], ['s-review', 's-done']);
  }

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
      local_ocr: ocrOn(),
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
    if (step === 'ocr' && !state.ocr) return 'The hardware is not detected yet.';
    if (step === 'startup' && !state.startup) return 'The start-up choices are not loaded yet.';
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
    ];
    if (f.cors_origins.length) rows.push(['CORS', f.cors_origins.join(', ')]);
    rows.push(['OCR here', ocrOn() && state.ocr ? 'yes: ' + state.ocr.describe() : 'no, processors only']);
    rows.push(['Start with the machine', startupOn() && state.startup ? state.startup.describe() : 'no']);
    $('summary').innerHTML = rows.map(([k, v]) => '<dt>' + esc(k) + '</dt><dd>' + esc(v) + '</dd>').join('');
  }

  // What the review step runs, in order.
  function stages() {
    const s = [['save', 'Save the configuration']];
    if (ocrOn()) s.push(['ocr', 'Install OCR'], ['doctor', 'Check (doctor)']);
    if (startupOn()) s.push(['startup', 'Start with the machine']);
    s.push(['start', 'Start the server']);
    return s;
  }

  let mark = () => {};

  async function run() {
    const btn = $('save-btn');
    const err = $('err-review');
    showError(err, null);
    $('skip-btn').hidden = true;
    failedStage = null;
    if (!finished.save) {
      $('run-list').hidden = false;
      mark = S.runList($('run-list'), stages());
    }
    setBusy(btn, true, 'Working…');
    $('review-back').disabled = true;
    for (const [k] of stages()) {
      if (finished[k]) continue;
      mark(k, 'running');
      let ok = false;
      try {
        ok = await stage(k);
      } catch (e) {
        showError(err, e);
        mark(k, 'failed', e.message && e.message.length < 60 ? e.message : '');
        ok = false;
      }
      if (!ok) {
        failedStage = k;
        setBusy(btn, false);
        btn.textContent = 'Try again';
        if (k === 'save') $('review-back').disabled = false;
        // OCR and the start-up can be done later from Settings.
        if (k === 'ocr' || k === 'startup') {
          $('skip-btn').hidden = false;
          $('skip-btn').textContent = k === 'ocr' ? 'Skip OCR for now' : 'Skip this';
        }
        return;
      }
      finished[k] = true;
    }
  }

  function skip() {
    if (!failedStage) return;
    finished[failedStage] = true;
    mark(failedStage, 'skipped', 'skipped: Settings can do it later');
    if (failedStage === 'ocr') { finished.doctor = true; mark('doctor', 'skipped'); }
    run();
  }

  async function stage(k) {
    if (k === 'save') {
      try {
        const r = await post('/app/api/server/setup', form());
        $('save-result').innerHTML = '<ul class="notes">' + r.notes.map((n) => '<li>' + esc(n) + '</li>').join('') + '</ul>';
        mark('save', 'ok');
        return true;
      } catch (e) {
        if (e.message && e.message.includes('already exists')) $('overwrite-row').hidden = false;
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
      let s;
      if (startupOn() && state.startup.startsIt()) {
        // The tray or the service starts it: wait for it to answer.
        const h = await S.waitFor(async () => { const x = await get('/app/api/server/health'); return x.up ? x : null; }, 60);
        s = h ? { url: h.url, up: true, by_startup: true } : await post('/app/api/server/start');
      } else {
        s = await post('/app/api/server/start');
      }
      if (!s.up) {
        mark('start', 'failed', 'no answer at ' + s.url);
        $('save-result').innerHTML += '<div class="result result--bad"><strong>The server did not answer at ' + esc(s.url) +
          '.</strong><pre class="log-view">' + esc(s.output || '') + '</pre><p class="form-hint">Its output: ' + esc(s.log || '') + '</p></div>';
        return false;
      }
      mark('start', 'ok', s.url);
      await done(s);
      return true;
    }
    return true;
  }

  async function done(s) {
    const i = await info(true);
    const admin = i.library_url + (i.admin_path || '/_admin');
    $('done-body').innerHTML =
      '<div class="result result--ok">' + (s.already_running ? 'A server was already running at ' : 'Answering at ') +
      '<a href="' + esc(s.url) + '" target="_blank" rel="noopener">' + esc(s.url) + '</a>.</div>' +
      (doctorOk ? '' : '<p class="form-hint mt-2">The check found problems: see <a href="/app/settings/doctor">Diagnostics</a>.</p>');
    const links = [
      '<li><a href="' + esc(s.url) + '" target="_blank" rel="noopener">Open the library</a></li>',
      '<li><a href="' + esc(admin) + '" target="_blank" rel="noopener">Open the admin panel</a> (users, invites, settings, tunnel)</li>',
      '<li><a href="#" id="go-dashboard">Go to its dashboard</a> <span class="form-hint" id="dash-note"></span></li>',
      '<li class="form-hint">OCR and start-up can change later: <a href="/app/settings/ocr">OCR &amp; models</a>, <a href="/app/settings/startup">Start-up</a>.</li>',
    ];
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
    const me = await S.waitFor(async () => {
      const r = await get('/app/api/instances');
      return r.instances.find((x) => x.role === 'server' && x.alive && x.dashboard);
    }, 10);
    if (me) {
      await post('/app/api/handoff', { url: me.dashboard }).catch(() => {});
      window.location.href = me.dashboard;
      return;
    }
    note.textContent = 'Its dashboard is not reachable (control API off?). Open the library instead.';
  }

  // The OCR toggle's line: on by default with a usable GPU.
  function ocrNote() {
    const i = state.info;
    if (!i.ocr_build) return 'Lite build: processors on other machines do the OCR.';
    if (!state.hw) return 'Could not detect the hardware.';
    const g = S.gpus(state.hw);
    if (S.usableGpu(state.hw)) return g + ' found.';
    return (g ? g + ' is not usable here. ' : 'No GPU found. ') + 'Processors on other machines can do the OCR.';
  }

  async function loadStartup() {
    try {
      state.startup = await S.startupStep($('startup-fields'), 'server');
    } catch (e) { showError($('err-startup'), e); }
  }

  document.addEventListener('DOMContentLoaded', async () => {
    wirePickers();
    w = wizard(steps, (id) => { if (id === 's-review') summary(); }, { done: 's-done' });
    document.querySelectorAll('[data-next]').forEach((b) => b.addEventListener('click', () => {
      const step = b.dataset.next;
      const msg = check(step);
      const err = $('err-' + step);
      if (err) showError(err, msg ? new Error(msg) : null);
      if (!msg) w.next();
    }));
    document.querySelectorAll('[data-prev]').forEach((b) => b.addEventListener('click', () => w.prev()));
    $('local-ocr').addEventListener('change', w.refresh);
    $('want-startup').addEventListener('change', () => {
      if (startupOn() && !state.startup) loadStartup();
      w.refresh();
    });
    $('make-admin').addEventListener('change', () => { $('admin-fields').hidden = !$('make-admin').checked; });
    document.querySelectorAll('input[name="access"]').forEach((r) => r.addEventListener('change', () => {
      $('dyndns-fields').hidden = val('access') !== 'dyndns';
    }));
    $('dd-provider').addEventListener('change', () => { $('dd-url-group').hidden = $('dd-provider').value !== 'generic'; });
    document.querySelectorAll('input[name="ssl"]').forEach((r) => r.addEventListener('change', () => {
      $('ssl-files').hidden = val('ssl') !== 'files';
    }));
    $('save-btn').addEventListener('click', run);
    $('skip-btn').addEventListener('click', skip);
    $('server-form').addEventListener('submit', (e) => e.preventDefault());

    try {
      const i = await info();
      state.info = i;
      $('storage').value = i.server_storage || '';
      if (!i.config_exists) window.App.storageNote($('storage-note'), $('storage'), i.server_storage_check);
      let fileOcr = null;
      if (i.config_exists) {
        const c = await get('/app/api/server/config');
        const f = c.file;
        $('storage').value = f.storage.base_path;
        $('port').value = f.server.port;
        const host = document.querySelector('input[name="host"][value="' + f.server.host + '"]');
        if (host) host.checked = true;
        const reg = document.querySelector('input[name="reg"][value="' + f.registration.mode + '"]');
        if (reg) reg.checked = true;
        fileOcr = !!f.ocr.local_processing;
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
      } else {
        try {
          state.hw = await get('/app/api/ocr/hardware');
          state.ocr = S.ocrStep($('ocr-fields'), 'server', state.hw);
        } catch (e) { showError($('err-ocr'), e); }
        $('local-ocr').checked = fileOcr != null ? fileOcr : S.usableGpu(state.hw);
      }
      $('ocr-note').textContent = ocrNote();
      w.refresh();
    } catch (e) {
      showError($('err-folder'), e);
    }
  });
})();
