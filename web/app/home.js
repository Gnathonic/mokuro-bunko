// Home: what this machine is set up as, what runs, where to go next.
(function () {
  'use strict';
  const { esc, get, info, showError } = window.App;

  async function load() {
    try {
      const i = await info(true);
      const parts = [];
      parts.push('mokuro-bunko ' + i.version + ' (' + i.flavor + ', ' + i.target + ').');
      if (!i.config_exists && !i.processor_config_exists) {
        parts.push('Nothing is set up yet: choose what this machine should do.');
      }
      document.getElementById('home-lede').textContent = parts.join(' ');
      document.getElementById('state-server').textContent = i.config_exists
        ? 'Set up: ' + i.config_path : 'Not set up';
      if (!i.ocr_build) {
        for (const id of ['choice-processor', 'choice-ocr']) {
          const el = document.getElementById(id);
          el.removeAttribute('href');
          el.setAttribute('aria-disabled', 'true');
          el.style.opacity = '0.5';
        }
        document.getElementById('state-processor').textContent = 'Needs the full build';
        document.getElementById('state-ocr').textContent = 'Needs the full build (this is lite)';
      } else {
        document.getElementById('state-processor').textContent = i.processor_config_exists
          ? 'Set up: ' + i.processor_config : 'Not set up';
        document.getElementById('state-ocr').textContent = 'Installs into ' + i.server_storage + ' or ' + i.processor_storage;
      }
      document.getElementById('state-startup').textContent = 'As ' + i.service_describe;

      const r = await get('/app/api/instances');
      const items = [];
      if (r.library && r.library.up) {
        items.push('<li>Library server answering at <a href="' + esc(r.library.url) + '" target="_blank" rel="noopener">' + esc(r.library.url) + '</a></li>');
      }
      r.instances.filter((x) => x.alive && x.role !== 'gui').forEach((x) => {
        const label = x.role === 'server' ? 'Library server' : 'Processor';
        items.push('<li>' + label + ' (pid ' + esc(x.pid) + ', ' + esc(x.version) + ')' +
          (x.dashboard ? ' — <a href="' + esc(x.dashboard) + '">open its dashboard</a>' : '') + '</li>');
      });
      document.getElementById('running-list').innerHTML = items.join('');
      document.getElementById('running-none').hidden = items.length > 0;
    } catch (e) {
      showError(document.getElementById('home-error'), e);
    }
  }
  document.addEventListener('DOMContentLoaded', load);
})();
