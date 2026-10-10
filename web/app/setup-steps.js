// The steps both setup flows share: OCR install and start with the machine (their
// choices are made in a step; they apply from the review step, after the configuration
// is saved), and the review step's list of what runs. The OCR backend itself installs
// in the background once the server or processor runs: the last page comes as soon as
// it serves and follows the install.
(function () {
  'use strict';
  const { esc, get, post, runJob, jobBox, wirePickers, PADDLE_CPU_NOTE } = window.App;
  const $ = (id) => document.getElementById(id);
  const roleName = (role) => (role === 'processor' ? 'processor' : 'server');

  // The GPUs install-ocr found, as one line ('' for none).
  function gpus(hw) {
    const g = [];
    if (hw.nvidia_driver != null) g.push('NVIDIA ' + (hw.nvidia_gpus.length ? hw.nvidia_gpus.join(', ') : 'driver ' + hw.nvidia_driver));
    if (hw.amd_gfx.length) g.push('AMD ' + hw.amd_gfx.join(', '));
    return g.join('; ');
  }

  // A GPU the automatic pack uses (not only one that is present).
  const usableGpu = (hw) => !!hw && hw.auto_variant !== 'cpu';

  // The OCR step: hardware, the pack, the advanced options. `el` gets the fields.
  function ocrStep(el, role, hw) {
    el.innerHTML =
      '<dl class="kv" id="ocr-hw"></dl>' +
      '<p class="form-hint mt-2" id="ocr-hint" hidden></p>' +
      '<div class="form-group mt-3">' +
      '<label class="form-label" for="ocr-variant">Backend pack</label>' +
      '<select class="form-select" id="ocr-variant">' +
      '<option value="auto">Automatic</option>' +
      '<option value="cpu">CPU (any machine)</option>' +
      '<option value="cu130">NVIDIA GPU (CUDA 13, driver 580 or newer)</option>' +
      '<option value="rocm7.1">AMD GPU (ROCm 7.1, Linux)</option>' +
      '</select>' +
      '<p class="form-hint" id="ocr-cpu-note" hidden></p>' +
      '</div>' +
      '<details class="mb-3"><summary class="form-label" style="cursor:pointer">More options</summary>' +
      '<div class="form-group mt-2">' +
      '<label class="form-label" for="ocr-from">Install from a folder instead of downloading</label>' +
      '<div class="input-row"><input class="form-input" id="ocr-from" placeholder="a folder with the pack archive and the models">' +
      '<button type="button" class="btn btn--secondary" data-pick="ocr-from" data-title="Folder with the pack archive">Browse…</button></div>' +
      '</div>' +
      '<label class="check"><input type="checkbox" id="ocr-no-models"> Only the pack, not the models</label>' +
      '<label class="check"><input type="checkbox" id="ocr-force"> Reinstall even if the pack is already there</label>' +
      '</details>';
    wirePickers(el);
    const g = gpus(hw);
    const mine = (hw.packs || []).filter((p) => p.role === role || p.role === 'bundled');
    $('ocr-hw').innerHTML =
      '<dt>GPU</dt><dd>' + esc(g || 'none found') + '</dd>' +
      '<dt>Recommended</dt><dd><strong>' + esc(hw.auto_variant) + '</strong> — ' + esc(hw.reason) + '</dd>' +
      (hw.bundled_offline ? '<dt>Source</dt><dd>Bundled with this app (nothing to download)</dd>' : '') +
      (mine.length ? '<dt>Installed</dt><dd>' + mine.map((p) => esc(p.name) + (p.complete ? '' : ' (incomplete)')).join(', ') + '</dd>' : '');
    $('ocr-hint').hidden = !hw.hint;
    $('ocr-hint').textContent = hw.hint || '';
    if (hw.bundled_offline) $('ocr-from').placeholder = 'Bundled with this app: ' + hw.bundled_offline;
    $('ocr-variant').querySelector('option[value="auto"]').textContent = 'Automatic (' + hw.auto_variant + ')';
    const cpuNote = () => {
      const v = $('ocr-variant').value;
      const cpu = v === 'cpu' || (v === 'auto' && hw.auto_variant === 'cpu');
      $('ocr-cpu-note').hidden = !cpu;
      $('ocr-cpu-note').textContent = cpu ? PADDLE_CPU_NOTE : '';
    };
    $('ocr-variant').addEventListener('change', cpuNote);
    cpuNote();
    return {
      request() {
        return {
          kind: 'install-ocr',
          variant: $('ocr-variant').value,
          from: $('ocr-from').value.trim() || null,
          no_models: $('ocr-no-models').checked,
          force: $('ocr-force').checked,
          processor: role === 'processor',
        };
      },
      describe() {
        const v = $('ocr-variant').value;
        const from = $('ocr-from').value.trim();
        return (v === 'auto' ? hw.auto_variant : v) + ' pack' +
          ($('ocr-no-models').checked ? ', no models' : ' and the models') +
          (from ? ', from ' + from : '');
      },
    };
  }

  // The start-up step: from the tray at login, or as a per-user service.
  async function startupStep(el, role) {
    const [tray, svc] = await Promise.all([get('/app/api/tray'), get('/app/api/service?role=' + role)]);
    const who = roleName(role);
    el.innerHTML =
      '<div class="radio-cards">' +
      '<label class="radio-card"><input type="radio" name="su-how" value="tray" id="su-tray"><div class="radio-card__content">' +
      '<strong>From the tray at login</strong><span id="su-tray-text">The tray starts the ' + who + ', restarts it if it stops, and can pause it.</span></div></label>' +
      '<label class="radio-card"><input type="radio" name="su-how" value="service" id="su-service"><div class="radio-card__content">' +
      '<strong>As a background service</strong><span>' + esc(svc.describe.charAt(0).toUpperCase() + svc.describe.slice(1)) + ', no tray.</span></div></label>' +
      '</div>' +
      '<label class="check mt-3"><input type="checkbox" id="su-now" checked> <span id="su-now-text"></span></label>' +
      '<p class="form-hint" id="su-note"></p>';
    const how = () => (el.querySelector('input[name="su-how"]:checked') || {}).value;
    if (!tray.available) {
      $('su-tray').disabled = true;
      $('su-tray-text').textContent = 'This build has no tray.';
    }
    $(tray.available && tray.recommended === 'tray' ? 'su-tray' : 'su-service').checked = true;
    let touched = false;
    $('su-now').addEventListener('change', () => { touched = true; });
    function render() {
      const t = how() === 'tray';
      const notes = [];
      $('su-now-text').textContent = t ? 'Start the tray now (it starts the ' + who + ')' : 'Start the service now';
      $('su-now').disabled = !t && !svc.can_start;
      if (!touched) $('su-now').checked = t ? !tray.headless : svc.can_start;
      if (!t && !svc.can_start) notes.push('No service manager runs here now: the file is only written.');
      if (!$('su-now').checked || $('su-now').disabled) notes.push('The setup starts the ' + who + ' itself this time.');
      if (t && svc.written) notes.push('Its service (' + svc.name + ') is removed, so only one copy runs.');
      if (!t && svc.tray_manages) notes.push('The tray stops running it, so only one copy runs.');
      $('su-note').textContent = notes.join(' ');
    }
    el.querySelectorAll('input[name="su-how"]').forEach((r) => r.addEventListener('change', render));
    $('su-now').addEventListener('change', render);
    render();
    const startsIt = () => $('su-now').checked && !$('su-now').disabled;
    return {
      startsIt,
      describe() {
        return (how() === 'tray' ? 'from the tray at login' : 'as ' + svc.describe) + (startsIt() ? ', started now' : '');
      },
      async apply() {
        const r = how() === 'tray'
          ? await post('/app/api/tray', { role, action: 'enable', autostart: true, start_now: startsIt(), remove_service: svc.written })
          : await post('/app/api/service', { role, action: 'install', start: startsIt(), remove_tray: svc.tray_manages });
        return r.messages || [];
      },
    };
  }

  // The review step's list of what runs: [key, label] pairs into `ul`.
  function runList(ul, stages) {
    ul.innerHTML = stages.map(([k, label]) =>
      '<li data-stage="' + esc(k) + '"><span>' + esc(label) + '<span class="form-hint run-note"></span></span></li>').join('');
    return (k, state, note) => {
      const li = ul.querySelector('li[data-stage="' + k + '"]');
      if (!li) return;
      li.dataset.state = state;
      li.querySelector('.run-note').textContent = note ? ' — ' + note : '';
    };
  }

  // The OCR step's choices go to the instance that is about to start: it installs the
  // backend in the background once it serves (nothing waits for the download).
  async function queueOcr(request, role) {
    const r = Object.assign({}, request, { role: role });
    delete r.kind;
    delete r.processor;
    return post('/app/api/ocr/request', r);
  }

  // One line for `status.install` (the background OCR backend install).
  function installLine(i) {
    if (!i) return null;
    if (i.state === 'done') return { ok: true, text: 'OCR backend installed' + (i.pack ? ' (' + i.pack + ')' : '') + '; local OCR uses it now.' };
    if (i.state === 'failed') return { bad: true, text: 'The OCR backend install failed' + (i.message ? ': ' + i.message : '') + '. Retry from the dashboard.' };
    if (i.state === 'missing') return { bad: true, text: (i.message || 'No OCR backend installed') + '. Install it from the dashboard.' };
    const what = { checking: 'checking what this machine needs', waiting: 'waiting for another install',
      downloading: 'downloading the ' + (i.variant ? i.variant + ' ' : '') + 'backend pack',
      unpacking: 'unpacking the backend pack', libraries: 'fetching NVIDIA\'s CUDA libraries',
      installed: 'backend pack installed', models: 'fetching the OCR models' }[i.stage] || i.stage;
    const bytes = i.done_bytes != null && i.total_bytes
      ? ' (' + (i.done_bytes / 1e9).toFixed(2) + ' of ' + (i.total_bytes / 1e9).toFixed(2) + ' GB)' : '';
    return { text: 'Installing the OCR backend in the background: ' + what + (i.percent != null ? ' ' + i.percent + '%' : '…') + bytes +
      '. The ' + 'server already serves; you can close this page.', percent: i.percent };
  }

  // Follow the started instance's OCR install on the last page (until it ends or
  // the setup app closes).
  function watchInstall(el, slot) {
    let timer = null;
    async function tick() {
      let s = null;
      try { s = await get('/app/api/instances/' + slot + '/status'); } catch (_) { /* not up yet */ }
      const line = s && installLine(s.install);
      el.hidden = !line;
      if (line) {
        el.className = 'result ' + (line.bad ? 'result--bad' : line.ok ? 'result--ok' : 'result--wait') + ' mt-2';
        el.textContent = line.text.replace('The server', slot === 'processor' ? 'The processor' : 'The server');
      }
      if (s && s.install && s.install.state !== 'running') { clearInterval(timer); timer = null; }
    }
    tick();
    timer = setInterval(tick, 2000);
  }

  // Wait for `check()` to give a truthy value (every half second, up to `secs`).
  async function waitFor(check, secs) {
    const end = Date.now() + secs * 1000;
    while (Date.now() < end) {
      try {
        const v = await check();
        if (v) return v;
      } catch (_) { /* not yet */ }
      await new Promise((r) => setTimeout(r, 500));
    }
    return null;
  }

  window.SetupSteps = { gpus, usableGpu, ocrStep, startupStep, runList, queueOcr, installLine, watchInstall, waitFor };
})();
