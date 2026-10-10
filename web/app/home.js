// Home: the chooser. Library server: the tray runs a server and the browser moves to
// its own setup page (then its admin panel). Processor: the local pairing page.
(function () {
  'use strict';
  const { esc, get, post, info, showError, setBusy, wirePickers } = window.App;
  const $ = (id) => document.getElementById(id);
  let i = null;

  async function startServer() {
    const btn = $('server-go');
    const status = $('server-status');
    setBusy(btn, true, 'Starting…');
    status.textContent = 'Starting the server…';
    try {
      const r = await post('/app/api/server/new', { storage: $('storage').value.trim() });
      if (!r.up || !r.open) {
        status.textContent = '';
        showError($('home-error'), new Error(r.error || ('The server did not start.' + (r.output ? '\n' + r.output : ''))));
        return;
      }
      status.textContent = 'Opening ' + r.open + '…';
      window.location.href = r.open;
    } catch (e) {
      status.textContent = '';
      showError($('home-error'), e);
    } finally {
      setBusy(btn, false);
    }
  }

  async function load() {
    try {
      i = await info(true);
      $('state-server').textContent = i.config_exists ? 'Set up · opens its admin panel' : '';
      if (!i.ocr_build) {
        const el = $('choice-processor');
        el.removeAttribute('href');
        el.setAttribute('aria-disabled', 'true');
        el.style.opacity = '0.5';
        $('state-processor').textContent = 'Needs the full build';
      } else if (i.processor_config_exists) {
        $('state-processor').textContent = 'Paired';
        $('choice-processor').href = '/app/settings';
      }
      $('storage').value = i.server_storage;
      window.App.storageNote($('storage-note'), $('storage'), i.server_storage_check);

      const r = await get('/app/api/instances');
      const items = [];
      if (r.library && r.library.up) {
        items.push('<li>Library server: <a href="' + esc(r.library.url) + '/_admin" target="_blank" rel="noopener">admin panel</a>' +
          ' · <a href="' + esc(r.library.url) + '" target="_blank" rel="noopener">' + esc(r.library.url) + '</a></li>');
      }
      r.instances.filter((x) => x.alive && x.role === 'processor').forEach((x) => {
        items.push('<li>Processor (' + esc(x.version) + ')' +
          (x.dashboard ? ' — <a href="' + esc(x.dashboard) + '">its status</a>' : '') + '</li>');
      });
      $('running-list').innerHTML = items.join('');
      $('running-panel').hidden = items.length === 0;
    } catch (e) {
      showError($('home-error'), e);
    }
  }

  document.addEventListener('DOMContentLoaded', () => {
    wirePickers();
    $('choice-server').addEventListener('click', (ev) => {
      ev.preventDefault();
      if (i && i.config_exists) { startServer(); return; }
      $('server-start').hidden = false;
      $('storage').focus();
    });
    $('server-go').addEventListener('click', startServer);
    load();
  });
})();
