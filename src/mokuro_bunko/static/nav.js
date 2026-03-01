(function () {
  'use strict';

  function getSessionUser() {
    const raw = sessionStorage.getItem('mokuro_user');
    if (!raw) return null;
    try {
      return JSON.parse(raw);
    } catch (_) {
      return null;
    }
  }

  function getSessionAuth() {
    return sessionStorage.getItem('mokuro_auth');
  }

  function link(label, href, currentKey, key) {
    const klass = key === currentKey ? 'btn btn--secondary btn--sm' : 'btn btn--ghost btn--sm';
    return '<a href="' + href + '" class="' + klass + '">' + label + '</a>';
  }

  async function fetchNavConfig() {
    try {
      const response = await fetch('/api/nav/config');
      if (!response.ok) throw new Error('bad status');
      return await response.json();
    } catch (_) {
      return {
        home_enabled: true,
        catalog_enabled: true,
        queue_show_in_nav: false,
        queue_public_access: true,
        registration_enabled: true,
      };
    }
  }

  async function renderMokuroHeaderNav(currentKey) {
    const nav = document.getElementById('header-nav');
    if (!nav) return;

    const config = await fetchNavConfig();
    const auth = getSessionAuth();
    const user = getSessionUser();
    const isAuthed = !!(auth && user);

    const showHome = config.home_enabled !== false;
    const showCatalog = !!config.catalog_enabled;
    const showQueue = !!config.queue_show_in_nav && (!!config.queue_public_access || isAuthed);

    const parts = [];
    if (showHome) parts.push(link('Home', '/', currentKey, 'home'));
    if (showCatalog) parts.push(link('Catalog', '/catalog', currentKey, 'catalog'));
    if (showQueue) parts.push(link('Queue', '/queue', currentKey, 'queue'));

    if (isAuthed) {
      if (user.role === 'admin') {
        parts.push(link('Admin', '/_admin', currentKey, 'admin'));
      }
      parts.push(link('Account', '/account', currentKey, 'account'));
      parts.push('<button onclick="logout()" class="btn btn--secondary btn--sm">Logout</button>');
    } else {
      parts.push(link('Login', '/login', currentKey, 'login'));
      if (config.registration_enabled) {
        parts.push(link('Register', '/register', currentKey, 'register'));
      }
    }

    nav.innerHTML = parts.join('');
  }

  window.renderMokuroHeaderNav = renderMokuroHeaderNav;

  document.addEventListener('DOMContentLoaded', function () {
    const nav = document.getElementById('header-nav');
    if (!nav) return;
    const current = nav.getAttribute('data-current');
    if (current) {
      renderMokuroHeaderNav(current);
    }
  });
})();
