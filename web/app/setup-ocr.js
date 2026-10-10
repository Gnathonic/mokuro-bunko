// OCR install: hardware, pack choice, install-ocr with live progress, then doctor.
(function () {
  'use strict';
  const { esc, get, info, showError, setBusy, wirePickers, runJob, jobBox, PADDLE_CPU_NOTE } = window.App;
  const $ = (id) => document.getElementById(id);
  const role = () => 'processor';
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
      '<dt>Recommended</dt><dd><strong>' + esc(hw.auto_variant) + '</strong> — ' + esc(hw.reason) + '</dd>' +
      '<dt>Source</dt><dd>' + (hw.bundled_offline
        ? '<strong>Bundled with this app</strong> (nothing to download)'
        : 'Download from the release') + '</dd>';
    // Bundled files: Install takes them on its own; the folder field stays for others.
    if (hw.bundled_offline) {
      $('from').placeholder = 'Bundled with this app: ' + hw.bundled_offline;
    }
    $('hw-hint').hidden = !hw.hint;
    $('hw-hint').textContent = hw.hint || '';
    const opt = $('variant').querySelector('option[value="auto"]');
    opt.textContent = 'Automatic (' + hw.auto_variant + ')';
    renderCpuNote();
    const packs = (hw.packs || []).filter((p) => p.role !== 'server');
    $('packs').innerHTML = packs.length
      ? '<p class="form-label">Installed</p><ul class="notes">' + packs.map((p) =>
        '<li>' + esc(p.name) + ' — ' + esc(p.dir) + (p.complete ? '' : ' <span class="badge badge--error">incomplete</span>') + '</li>').join('') + '</ul>'
      : '<p class="form-hint">No pack installed yet.</p>';
  }

  // The CPU pack (picked, or what Automatic means here): hayai-nova is its engine.
  function renderCpuNote() {
    const v = $('variant').value;
    const cpu = v === 'cpu' || (v === 'auto' && !!hw && hw.auto_variant === 'cpu');
    $('cpu-note').hidden = !cpu;
    $('cpu-note').textContent = cpu ? PADDLE_CPU_NOTE : '';
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
    $('variant').addEventListener('change', renderCpuNote);
    try {
      const i = await info();
      if (!i.ocr_build) {
        showError($('err-install'), new Error('This is the lite build: OCR runs on processors (the full build) on other machines.'));
        $('install-btn').disabled = true;
        $('hw').innerHTML = '<dt>Build</dt><dd>lite</dd>';
        return;
      }
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
