// Start with the machine: from the tray (tray.json + the tray's login item) or as a
// per-user service — one or the other per role, never both.
(function () {
  'use strict';
  const { esc, get, post, info, showError, setBusy } = window.App;
  const $ = (id) => document.getElementById(id);
  const role = () => (document.querySelector('input[name="role"]:checked') || {}).value;
  const how = () => (document.querySelector('input[name="how"]:checked') || {}).value;
  const roleName = (r) => (r === 'processor' ? 'processor' : 'library server');
  let svc = null;
  let tray = null;

  function render() {
    const t = how() === 'tray';
    $('tray-opts').hidden = !t;
    $('svc-opts').hidden = t;
    if (!svc || !tray) return;
    const managed = tray.managed.map((m) => m.role);
    $('tray-kv').innerHTML =
      '<dt>Tray</dt><dd>' + esc(tray.tray_exe || 'not in this package') + (tray.running ? ' <span class="badge badge--success">running</span>' : '') + '</dd>' +
      '<dt>tray.json</dt><dd>' + esc(tray.config_path) + (managed.length ? ' — runs: ' + esc(managed.join(', ')) : '') + '</dd>' +
      '<dt>Login item</dt><dd>' + esc(tray.autostart_path || '-') + (tray.autostart ? ' <span class="badge badge--success">present</span>' : '') + '</dd>' +
      '<dt>Config</dt><dd>' + esc(svc.config) + (svc.config_exists ? '' : ' — <strong>not set up yet</strong>') + '</dd>';
    // A running tray reads tray.json only when it starts.
    $('tray-start').parentElement.lastChild.textContent = tray.running
      ? ' Restart the tray now, so it picks this up'
      : ' Start the tray now';
    // A service for this role would run a second copy.
    $('tray-conflict-row').hidden = !svc.written;
    $('tray-conflict-text').textContent = 'Remove the ' + roleName(role()) + "'s service (" + svc.name + '): it would start a second copy';
    $('svc-conflict-row').hidden = !svc.tray_manages;
    $('svc-conflict-text').textContent = 'Stop the tray from running the ' + roleName(role()) + ': it would start a second copy' +
      (tray.running ? ' (the tray restarts to pick that up)' : '');

    $('svc-kv').innerHTML =
      '<dt>As</dt><dd>' + esc(svc.describe) + ' (' + esc(svc.name) + ')</dd>' +
      '<dt>File</dt><dd>' + esc(svc.path) + '</dd>' +
      '<dt>Config</dt><dd>' + esc(svc.config) + (svc.config_exists ? '' : ' — <strong>not set up yet</strong>') + '</dd>' +
      '<dt>Now</dt><dd id="svc-state">' + esc(svc.written ? ('written' + (svc.enabled ? ', ' + svc.enabled : '')) : 'not set up') + '</dd>';
    $('svc-text').textContent = svc.text;
    if (!svc.can_start) {
      $('start-now').checked = false;
      $('start-now').disabled = true;
      $('start-note').textContent = 'No running service manager here: the file is written, and starts it at the next login.';
    } else {
      $('start-now').disabled = false;
      $('start-note').textContent = '';
    }
    const trayOn = managed.includes(role());
    $('remove-btn').hidden = t ? !trayOn : !svc.written;
    $('remove-btn').textContent = t ? 'Stop running it from the tray' : 'Remove the service';
    $('install-btn').disabled = !svc.config_exists || (t && !tray.available);
  }

  async function load() {
    showError($('err-svc'), null);
    try {
      [svc, tray] = await Promise.all([
        get('/app/api/service?role=' + encodeURIComponent(role())),
        get('/app/api/tray'),
      ]);
      $('kind-desc').textContent = 'Starts at login as ' + svc.describe + ', with or without a tray.';
      render();
    } catch (e) { showError($('err-svc'), e); }
  }

  function result(r) {
    $('svc-result').innerHTML = '<div class="result result--ok"><ul class="notes">' +
      r.messages.map((m) => '<li>' + esc(m) + '</li>').join('') + '</ul></div>';
  }

  async function apply() {
    const btn = $('install-btn');
    setBusy(btn, true);
    showError($('err-svc'), null);
    try {
      let r;
      if (how() === 'tray') {
        r = await post('/app/api/tray', {
          role: role(), action: 'enable',
          autostart: $('tray-autostart').checked,
          start_now: $('tray-start').checked,
          remove_service: !$('tray-conflict-row').hidden && $('tray-remove-service').checked,
        });
      } else {
        r = await post('/app/api/service', {
          role: role(), action: 'install', start: $('start-now').checked,
          remove_tray: !$('svc-conflict-row').hidden && $('svc-remove-tray').checked,
        });
      }
      result(r);
      await load();
    } catch (e) {
      showError($('err-svc'), e);
    } finally { setBusy(btn, false); }
  }

  async function remove() {
    const btn = $('remove-btn');
    setBusy(btn, true);
    showError($('err-svc'), null);
    try {
      const r = how() === 'tray'
        ? await post('/app/api/tray', { role: role(), action: 'disable' })
        : await post('/app/api/service', { role: role(), action: 'remove' });
      result(r);
      await load();
    } catch (e) {
      showError($('err-svc'), e);
    } finally { setBusy(btn, false); }
  }

  document.addEventListener('DOMContentLoaded', async () => {
    try {
      const [i, t] = await Promise.all([info(), get('/app/api/tray')]);
      $('role-server-cfg').textContent = i.config_path + (i.config_exists ? '' : ' (not set up)');
      $('role-proc-cfg').textContent = i.ocr_build
        ? i.processor_config + (i.processor_config_exists ? '' : ' (not set up)') : 'needs the full build';
      if (!i.ocr_build) document.querySelector('input[name="role"][value="processor"]').disabled = true;
      const want = new URLSearchParams(location.search).get('role') ||
        (i.processor_config_exists && !i.config_exists ? 'processor' : 'server');
      const r = document.querySelector('input[name="role"][value="' + want + '"]');
      if (r && !r.disabled) r.checked = true;
      // The tray where there is a desktop and a tray program; a service elsewhere.
      $('rec-' + t.recommended).hidden = false;
      $('how-' + t.recommended).checked = true;
      if (!t.available) {
        $('how-tray').disabled = true;
        $('how-tray-text').textContent = 'This package has no tray program (mokuro-bunko-tray) next to mokuro-bunko.';
      } else if (t.headless) {
        $('how-tray-text').textContent += ' This session has no desktop, so a service suits it better.';
      }
    } catch (e) { showError($('err-svc'), e); }
    document.querySelectorAll('input[name="role"]').forEach((x) => x.addEventListener('change', () => {
      $('svc-result').innerHTML = '';
      load();
    }));
    document.querySelectorAll('input[name="how"]').forEach((x) => x.addEventListener('change', render));
    $('install-btn').addEventListener('click', apply);
    $('remove-btn').addEventListener('click', remove);
    load();
  });
})();
