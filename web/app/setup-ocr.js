// OCR install: hardware, pack choice, install-ocr with live progress, then doctor.
(function () {
  'use strict';
  const { esc, get, info, showError, setBusy, wirePickers, runJob, jobBox } = window.App;
  const $ = (id) => document.getElementById(id);
  const role = () => (document.querySelector('input[name="for"]:checked') || {}).value;
  let hw = null;

  function renderHardware() {
    if (!hw) return;
    const gpus = [];
    if (hw.nvidia_driver != null) gpus.push('NVIDIA driver ' + (hw.nvidia_driver || 'present') + (hw.nvidia_gpus.length ? ' (' + hw.nvidia_gpus.join(', ') + ')' : ''));
    if (hw.amd_gfx.length) gpus.push('AMD ' + hw.amd_gfx.join(', '));
    $('hw').innerHTML =
      '<dt>Platform</dt><dd>' + esc(hw.target) + '</dd>' +
      '<dt>GPU</dt><dd>' + esc(gpus.length ? gpus.join('; ') : 'none found (the CPU pack works everywhere)') + '</dd>' +
      (hw.hidden.length ? '<dt>Hidden</dt><dd>' + esc(hw.hidden.join(', ')) + '</dd>' : '') +
      '<dt>Recommended</dt><dd><strong>' + esc(hw.auto_variant) + '</strong> — ' + esc(hw.reason) + '</dd>';
    $('hw-hint').hidden = !hw.hint;
    $('hw-hint').textContent = hw.hint || '';
    const opt = $('variant').querySelector('option[value="auto"]');
    opt.textContent = 'Automatic (' + hw.auto_variant + ')';
    const packs = hw.packs || [];
    $('packs').innerHTML = packs.length
      ? '<p class="form-label">Installed</p><ul class="notes">' + packs.map((p) =>
        '<li>' + esc(p.name) + ' for the ' + esc(p.role) + ' — ' + esc(p.dir) + (p.complete ? '' : ' <span class="badge badge--error">incomplete</span>') + '</li>').join('') + '</ul>'
      : '<p class="form-hint">No pack installed yet.</p>';
  }

  async function install() {
    const btn = $('install-btn');
    showError($('err-install'), null);
    setBusy(btn, true, 'Installing…');
    $('ocr-done').hidden = true;
    $('doctor-job').innerHTML = '';
    const req = {
      kind: 'install-ocr',
      variant: $('variant').value,
      from: $('from').value.trim() || null,
      dir: $('dir').value.trim() || null,
      force: $('force').checked,
      no_models: $('no-models').checked,
      processor: role() === 'processor',
    };
    try {
      const box = jobBox($('install-job'));
      const r = await runJob(req, box);
      if (r.state === 'ok') {
        const d = jobBox($('doctor-job'));
        await runJob({ kind: 'doctor', processor: role() === 'processor' }, d);
        try { hw = await get('/app/api/ocr/hardware'); renderHardware(); } catch (_) {}
        const links = [
          '<li><a href="/app/setup/startup?role=' + esc(role()) + '">Start the ' + (role() === 'processor' ? 'processor' : 'library server') + ' with the machine</a></li>',
          '<li><a href="/app/settings/ocr">OCR settings and models</a></li>',
          '<li class="form-hint">The first volume of each engine measures its speed on this machine (the benchmark the scheduler uses).</li>',
        ];
        $('ocr-next').innerHTML = links.join('');
        $('ocr-done').hidden = false;
      }
    } catch (e) {
      showError($('err-install'), e);
    } finally {
      setBusy(btn, false);
    }
  }

  document.addEventListener('DOMContentLoaded', async () => {
    wirePickers();
    $('install-btn').addEventListener('click', install);
    try {
      const i = await info();
      if (!i.ocr_build) {
        showError($('err-install'), new Error('This is the lite build: OCR runs on processors (the full build) on other machines.'));
        $('install-btn').disabled = true;
        $('hw').innerHTML = '<dt>Build</dt><dd>lite</dd>';
        return;
      }
      $('for-server-path').textContent = 'Into ' + i.server_storage;
      $('for-proc-path').textContent = 'Into ' + i.processor_storage + (i.processor_config_exists ? '' : ' (not set up yet)');
      const want = new URLSearchParams(location.search).get('role') ||
        (i.processor_config_exists && !i.config_exists ? 'processor' : 'server');
      const radio = document.querySelector('input[name="for"][value="' + want + '"]');
      if (radio) radio.checked = true;
      hw = await get('/app/api/ocr/hardware');
      renderHardware();
      // A running install (page reloaded): show it again.
      const jobs = await get('/app/api/jobs');
      const running = jobs.jobs.find((j) => j.kind === 'install-ocr' && j.state === 'running');
      if (running) {
        setBusy($('install-btn'), true, 'Installing…');
        App.attachJob(running, jobBox($('install-job'))).then(() => setBusy($('install-btn'), false));
      }
    } catch (e) {
      showError($('err-install'), e);
    }
  });
})();
