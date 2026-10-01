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

  // Signed-in state is a bearer token (`POST /login/api/token`), never the
  // password: a page keeps `mokuro_token` and sends `Authorization: Bearer`.
  // `mokuro_auth` held base64 username:password before tokens -- dropped on
  // sight, which signs such a tab out once.
  sessionStorage.removeItem('mokuro_auth');

  const TOKEN_KEY = 'mokuro_token';
  const USER_KEY = 'mokuro_user';

  function getSessionAuth() {
    return sessionStorage.getItem(TOKEN_KEY);
  }

  const mokuroAuth = {
    token: getSessionAuth,
    // Headers carrying the token, or none when signed out.
    headers: function () {
      const token = getSessionAuth();
      return token ? { Authorization: 'Bearer ' + token } : {};
    },
    // Check the password once and keep the token it buys. Throws with the
    // server's message on failure.
    signIn: async function (username, password) {
      const response = await fetch('/login/api/token', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ username: username, password: password, kind: 'web', label: 'web page' }),
      });
      let data = {};
      try { data = await response.json(); } catch (_) { data = {}; }
      if (!response.ok || !data.token) {
        throw new Error(data.error || 'Authentication failed');
      }
      sessionStorage.setItem(TOKEN_KEY, data.token);
      sessionStorage.setItem(USER_KEY, JSON.stringify(data.user));
      return data;
    },
    // Forget the token here (nothing is sent: the server may be unreachable).
    clear: function () {
      sessionStorage.removeItem(TOKEN_KEY);
      sessionStorage.removeItem(USER_KEY);
      sessionStorage.removeItem('mokuro_auth');
    },
    // Revoke the token on the server, then forget it.
    signOut: async function () {
      const headers = mokuroAuth.headers();
      mokuroAuth.clear();
      if (headers.Authorization) {
        try {
          await fetch('/login/api/token', { method: 'DELETE', headers: headers });
        } catch (_) { /* signed out here either way */ }
      }
    },
  };
  window.mokuroAuth = mokuroAuth;

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
      } else if (user.role === 'inviter') {
        parts.push(link('Invites', '/_admin', currentKey, 'admin'));
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
